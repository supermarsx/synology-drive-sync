# Testing and coverage

The normal local gate mirrors the Rust portions of CI:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-targets
cargo build --release --locked -p synology-drive-sync
cargo build --profile ffi-release --locked -p synology-drive-sync-ffi
```

The CLI uses the workspace release profile. Build the C ABI with the dedicated unwind-safe `ffi-release` profile.

## Test layers

The deterministic suite has these complementary layers:

- **Unit tests** exercise configuration and credential-source precedence, path and deletion safety,
  hashing and planning, progress and observability failures, retry/cancellation behavior,
  reconciliation, batch completion decisions, and human/JSON/NDJSON schemas.
- **Command E2E tests** launch the built CLI and verify help, configuration, completion and manpage
  generation, source diagnostics, batch ordering, exit codes, stderr/stdout separation, and secret
  redaction.
- **Stateful File Station mock E2E tests** run real CLI flows against a local HTTP server that
  models reverse-proxy prefixes and discovery fallback, authentication/TOTP and independent
  sessions, inventory and permissions, disposable write-probe cleanup, all-target batch preflight,
  additive and destructive parity, guarded type-conflict replacement, verified server-side rename
  copies, retry reconciliation, and final rescan/replan checks. They also cover the failure
  vocabulary: a file that changes while it is hashed (exit `3`, `outcome: "partial"`, the deferral
  carried to the next run), a source that vanishes after upload, DSM session expiry with exactly one
  re-login, paginated and Advanced-Permission-hidden listings, a server-side copy that finishes with an
  error, and the guarantee that a case-only rename is one File Station rename that deletes nothing (a
  refused or blocked rename becomes a named `case-rename-pending` warning, never a delete) and that a
  Unicode-paired name is never deleted when its upload is deferred. `tests/scan_skip_protection.rs`
  walks and plans a real tree holding a link, SYSTEM files and OS metadata files and asserts that only
  the genuinely remote-only file is deleted. `tests/engine_adversarial.rs` and
  `tests/file_station_remote.rs` drive the real binary against the mock with the failures a
  production NAS produces per path (a name DSM refuses, a remote symbolic link, a name the walk cannot
  address, a share-wide refusal, an exclude that must protect every spelling) and assert that the run
  names the path, deletes nothing on its account and lets its siblings finish.
- **Dashboard surface tests** (`packaging/synology/ui-src/test/outcomes-surface.test.mjs` and the
  widget and AppWindow contract tests) feed every outcome, every reason code, unknown tokens and
  hostile input through the pure `outcomes.mjs` model and assert that nothing renders as plain success
  when a run failed, died, was partial, or the bridge stopped answering.

These layers use local filesystem fixtures and mock HTTP servers. They do not log in to a live NAS,
mutate the host credential vault, or prove compatibility with a particular DSM, File Station, TLS,
or reverse-proxy release. A disposable target on the intended NAS must still pass the steps in
[Production acceptance](production-acceptance.md), and the
[questions only a real NAS can answer](production-acceptance.md#questions-only-a-real-nas-can-answer)
list the behaviours the mock can only assume.

## Reproducible source coverage

Coverage uses the exact Rust and `cargo-llvm-cov` versions in the tracked
[`coverage.env`](https://github.com/supermarsx/synology-drive-sync/blob/main/.config/coverage.env).
Install those prerequisites before running a report:

```bash
rustup toolchain install 1.88.0 --component llvm-tools-preview
cargo +1.88.0 install cargo-llvm-cov --version 0.9.0 --locked
```

Run the full report or the same hard check used by CI:

```bash
bash scripts/coverage.sh report
bash scripts/coverage.sh check
```

PowerShell equivalents are:

```powershell
pwsh -File scripts/coverage.ps1 -Mode Report
pwsh -File scripts/coverage.ps1 -Mode Check
```

Use `html`/`-Mode Html` for a browsable report. Every mode cleans prior instrumentation, runs
`cargo llvm-cov --locked --workspace --all-targets`, and writes the machine-readable, unfiltered
summary to `target/llvm-cov/coverage-summary.json`. The Bash gate uses Python 3 to validate that
JSON, and the PowerShell gate invokes the same validator; Python 3.8 or newer is therefore required
for both entrypoints. Coverage artifacts stay under ignored `target/` paths.

The CI policy partitions the same unfiltered report into two non-overlapping line gates:

- **90% minimum** across every instrumented file except the exact DSM bridge boundary below.
- **74% minimum** across exactly `src/dsm_api.rs` and `src/bin/sdsync-dsm-api.rs`, combined.

The validator requires both DSM paths exactly once, rejects duplicate or out-of-repository paths,
and proves the two partitions add back to the unfiltered aggregate before comparing integer covered
and executable-line counts. No production file disappears from the JSON, terminal report, or
coverage policy. The separate DSM floor makes regression visible for the Linux-only package bridge
without allowing its larger platform boundary to mask the 90% repository-core floor. Inline
`#[cfg(test)]` code remains measured. Doctests are not included because stable `cargo-llvm-cov`
treats doctest coverage as experimental.

## Verified pinned result

On 2026-08-30, the PowerShell `Check` path passed with the pinned Rust 1.88.0 toolchain and
`cargo-llvm-cov 0.9.0`:

```powershell
pwsh -File scripts/coverage.ps1 -Mode Check
```

- Non-DSM lines: **20,167 / 21,576 = 93.4696%**
- DSM boundary lines: **5,086 / 5,775 = 88.0693%**
- Unfiltered total lines: **25,253 / 27,351 = 92.3293%**
- Tests: **500 passed** across all targets

This is a point-in-time snapshot, not a claim that today's repository still measures exactly this.
CI regenerates `coverage-summary.json` on every run rather than trusting a committed report, and
`grep -rc '#\[test\]' src tests | awk -F: '{sum+=$2} END {print sum}'` already counts **887** test
functions in `src/` and `tests/` alone as of this revision — noticeably more than the 500 this
snapshot measured, because tests have landed since 2026-08-30 without a matching re-run of this
block. Re-run `scripts/coverage.ps1 -Mode Check` for a current coverage percentage; a source-level
`#[test]` count is not a substitute for one, since it says nothing about which lines those tests
actually exercise.

For historical context, the pre-expansion audit used Rust 1.95 and the previously installed
`cargo-llvm-cov 0.6.11`; it measured **66.40% lines** (8,362 covered of 12,594 executable lines,
4,232 missed). That figure is retained only as the historical baseline, not the current pinned gate.

Coverage is evidence that code ran, not proof that the asserted behavior is correct. Do not lower
either threshold or exclude production files; retain the behavioral safety assertions and live-NAS
acceptance boundary.
