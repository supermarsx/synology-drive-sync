# synology-drive-sync

**A lean, one-way file deposit to a Synology NAS.** It pushes an ordinary local folder to a chosen
File Station path over HTTPS, verifies the result, logs out and exits. Use it as a command-line
program or embed the same engine as a Rust library.

It talks to DSM's documented **File Station Web API**, the same service the File Station web app
uses. It is not a Synology Drive protocol client, and it does not need Synology Drive Client on
the machine that sends the files.

## When to use it

Use it when you need to get files onto a Synology NAS from a machine where **Synology Drive Client
is not supported, not permitted, or not wanted**: a locked-down workstation, a build server, a
headless Linux box, another NAS, a container or a scheduled job.

- **One-way deposit, not two-way sync.** The local folder is authoritative. An edit made on the
  NAS is never brought back, and the next local change to that path overwrites it.
- **Reaches the NAS the way you already do.** Point it at a DSM HTTPS address, a reverse proxy, or
  a QuickConnect address. QuickConnect is supported in both its relay and direct forms; the relay
  is slower, so prefer the LAN address or the direct form where you have one, and `doctor` tells
  you which path a run is on.
- **Synology Drive can still pick the files up.** When the destination is the account's My Drive
  or an enabled Team Folder, Drive indexes what File Station wrote.
- **A finite process, not a daemon.** Every run scans, plans, transfers, verifies and exits, so
  cron, systemd, Task Scheduler, launchd, CI and containers drive it naturally.

It is not a backup tool: it sends the current state one way and keeps no history of what it
replaced. It does not carry ACLs, ownership, modes or extended attributes, only content,
hierarchy, names and modification time.

## Install

The command-line program:

```bash
cargo install synology-drive-sync
```

`cargo install` also installs `sdsync-dsm-api`, the helper the DSM package runs on a NAS; on a
workstation you only need `synology-drive-sync`.

The Rust SDK, as a library dependency:

```bash
cargo add synology-drive-sync
```

Releases are calendar-numbered `YY.N` upstream and published here as `0.YY.N` (release 26.66 is
crate `0.26.66`). Cargo treats every `0.26.x` as compatible, so pin an exact version with
`synology-drive-sync = "=0.26.66"` when API stability matters.

Prebuilt binaries, a container image and a DSM 7 package (`.spk`) with a dashboard are published
with each [GitHub release](https://github.com/supermarsx/synology-drive-sync/releases/latest),
with checksums and provenance. The C ABI for other languages is the separate
[`synology-drive-sync-ffi`](https://crates.io/crates/synology-drive-sync-ffi) crate.

## Use the CLI

Create a dedicated, non-administrator DSM account with File Station permission and read/write
access to the destination, store its password in the operating system's credential vault, then
preview before you sync. The destination is a File Station logical path such as
`/TeamShare/project`, never a physical `/volume1/...` path.

```bash
synology-drive-sync credentials set-password \
  --url https://nas.example.com --username deposit-bot

synology-drive-sync plan ./project /TeamShare/project \
  --url https://nas.example.com --username deposit-bot

synology-drive-sync sync ./project /TeamShare/project \
  --url https://nas.example.com --username deposit-bot
```

`plan`, `doctor source` and `doctor target` (without the separate `--write-test` opt-in) never
change the destination. Run `synology-drive-sync --help` for the rest.

## Use the Rust SDK

`sdk::Engine` is the supported entry point. It owns the order of every step: a caller can inspect
the immutable plan and decide whether to apply it, but cannot reach a remote change without the
scanner and planner having run first. This example previews only. It compiles as part of this
crate's documentation tests, but needs a NAS to run.

```rust,no_run
use synology_drive_sync::cancel::CancellationToken;
use synology_drive_sync::sdk::{
    Engine, EventControl, OtpChallenge, PlanDecision, Secret, SecretProvider,
    SecretProviderError, SyncRequest,
};

struct Secrets;

impl SecretProvider for Secrets {
    fn password(&mut self) -> Result<Secret, SecretProviderError> {
        // Fetch from your own secret store; this crate never reads the environment for you.
        std::env::var("DSM_PASSWORD")
            .map(Secret::new)
            .map_err(|_| SecretProviderError::Unavailable)
    }

    fn otp(&mut self, _challenge: OtpChallenge) -> Result<Option<Secret>, SecretProviderError> {
        Ok(None) // asked only if DSM demands a second factor
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let request = SyncRequest::builder(
        "https://nas.example.com", // DSM address, or a QuickConnect address
        "deposit-bot",             // a dedicated, non-administrator DSM account
        "./export",                // local source folder
        "/TeamShare/Project",      // File Station path, never /volume1/...
    )
    .exclude("*.tmp")
    .build()?;

    let outcome = Engine.run(
        &request,
        &mut Secrets,
        &CancellationToken::default(),
        |plan| {
            println!("{} planned changes, {} bytes to upload", plan.changes().len(), plan.upload_bytes());
            PlanDecision::PreviewOnly // return PlanDecision::Apply to perform it
        },
        |_event| EventControl::Continue,
    )?;
    assert!(!outcome.applied());
    Ok(())
}
```

A complete interactive example with prompts and an `--apply` switch is
[`examples/sdk/basic.rs`](https://github.com/supermarsx/synology-drive-sync/blob/main/examples/sdk/basic.rs).
The [SDK guide](https://supermarsx.github.io/synology-drive-sync/sdk/index.html) covers progress
events, error codes and cancellation, and the
[API documentation](https://docs.rs/synology-drive-sync) is on docs.rs.

## Safety and guarantees

- **Additive by default.** Remote-only entries are kept. Deletion needs an explicit, bounded
  policy at every layer that applies.
- **Plan first.** The complete plan exists, and your code or the `plan` command can inspect it,
  before the first remote change.
- **The source is read-only.** The local folder is opened read-only and is never intentionally
  changed.
- **Content comparison by default.** A same-path file matches on size and the NAS's MD5;
  modification time alone never triggers an upload, and uploads are verified.
- **Secrets stay yours.** Passwords and one-time codes arrive through a callback into zeroizing
  buffers and are never logged or written to configuration files.
- **Honest limits.** A file's own upload is crash-atomic (`deposit = "atomic"`, the default): while
  a file is being replaced its name is missing for one request, and a crash at that moment leaves
  the old version whole beside it until the next run puts it back (see
  [Crash-atomic deposit](https://supermarsx.github.io/synology-drive-sync/deposit.html)). Deletion,
  type replacement and a run as a whole are **not crash-atomic**, and a plan is not a backup: keep
  independent, tested recovery.
- **Young software.** It has been exercised against few NAS models and DSM builds, and the
  automated tests use mock servers rather than a live NAS. Rehearse against a disposable
  destination before you point it at data you would miss. See the
  [security notes](https://github.com/supermarsx/synology-drive-sync/blob/main/security.md) and
  the [acceptance runbook](https://supermarsx.github.io/synology-drive-sync/production-acceptance.html).

## Links

- Documentation: <https://supermarsx.github.io/synology-drive-sync/>
- API reference: <https://docs.rs/synology-drive-sync>
- Source and issues: <https://github.com/supermarsx/synology-drive-sync>
- Releases, binaries and the DSM package: <https://github.com/supermarsx/synology-drive-sync/releases>
- C ABI crate: <https://crates.io/crates/synology-drive-sync-ffi>

## License and trademarks

MIT licensed; see the
[license](https://github.com/supermarsx/synology-drive-sync/blob/main/license.md).

Synology, DSM, File Station, QuickConnect and Synology Drive are trademarks of Synology Inc. This
is an independent open-source project. It is not affiliated with, endorsed by or supported by
Synology.
