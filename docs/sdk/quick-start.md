# Rust quick start

## Add the dependency

Every calendar release `YY.N` is also published to [crates.io](https://crates.io/crates/synology-drive-sync)
as crate version `0.YY.N` (release 26.66 is `0.26.66`; when the year rolls over, 27.1 becomes
`0.27.1`). Cargo treats every `0.26.x` as compatible, so pin the exact version you verified:

```toml
[dependencies]
synology-drive-sync = "=0.YY.N"
```

`cargo add synology-drive-sync` adds the latest release; the API reference for each version is on
[docs.rs](https://docs.rs/synology-drive-sync). Publishing to crates.io is switched on by the
maintainer (see [crates.io packages](../releases.md#cratesio-packages)); for a release that crates.io
does not list, or for an unpublished commit, pin the calendar tag from Git instead:

```toml
[dependencies]
synology-drive-sync = { git = "https://github.com/supermarsx/synology-drive-sync", tag = "YY.N" }
```

The same release also contains `synology-drive-sync-YY.N-rust-sdk.tar.gz` for vendored/offline source
review. The Rust import uses underscores:

```rust,ignore
use synology_drive_sync::Result;
```

## Preview one complete run

Implement `SecretProvider` so secrets enter only at the authentication boundary, build an additive
HTTPS/content request, and return `PreviewOnly` after inspecting the immutable plan:

```rust,ignore
use synology_drive_sync::cancel::CancellationToken;
use synology_drive_sync::sdk::{
    Engine, EventControl, OtpChallenge, PlanDecision, Secret, SecretProvider,
    SecretProviderError, SyncRequest,
};

struct Secrets {
    password: String,
    current_otp: Option<String>,
}

impl SecretProvider for Secrets {
    fn password(&mut self) -> Result<Secret, SecretProviderError> {
        Ok(Secret::new(self.password.clone()))
    }

    fn otp(
        &mut self,
        _challenge: OtpChallenge,
    ) -> Result<Option<Secret>, SecretProviderError> {
        Ok(self.current_otp.take().map(Secret::new))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let request = SyncRequest::builder(
        "https://files.example.com",
        "mirror-bot",
        "./export",
        "/TeamShare/Project",
    )
    .exclude("*.tmp")
    .jobs(2)
    .build()?;

    let mut secrets = Secrets {
        password: "obtain-from-your-secret-store".to_owned(),
        current_otp: None,
    };
    let cancellation = CancellationToken::default();
    let outcome = Engine.run(
        &request,
        &mut secrets,
        &cancellation,
        |plan| {
            println!("{} planned changes", plan.changes().len());
            PlanDecision::PreviewOnly
        },
        |_event| EventControl::Continue,
    )?;

    assert!(!outcome.applied());
    Ok(())
}
```

The `Secret` wrapper redacts `Debug` and zeroizes its owned string on drop. A production provider
should fetch the password and OTP from an OS vault, protected file, prompt, or application-owned
secret manager instead of embedding a literal.

To mutate, return `PlanDecision::Apply` only after your application has displayed and approved the
plan. Deletion remains disabled unless the request explicitly receives a positive bounded
`DeletionPolicy`.

## Before making remote calls

Prefer `sdk::Engine`. The lower-level `api`, `plan`, and `sync` modules exist for the CLI, tests, and
specialist integrations, but composing them directly makes the caller responsible for every ordering,
freshness, deletion, reconciliation, and logout invariant that `Engine` already enforces.

The [generated API reference](api-reference.md) is built from the exact commit that produced this
site.
