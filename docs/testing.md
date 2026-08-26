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

The deterministic suite has three complementary layers:

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
  copies, retry reconciliation, and final rescan/replan checks.

These layers use local filesystem fixtures and mock HTTP servers. They do not log in to a live NAS,
mutate the host credential vault, or prove compatibility with a particular DSM, File Station, TLS,
or reverse-proxy release. A disposable target on the intended NAS must still pass the steps in
[Production acceptance](production-acceptance.md).

## Reproducible source coverage

Coverage uses the exact Rust and `cargo-llvm-cov` versions in the tracked
[`coverage.env`](https://github.com/supermarsx/synology-drive-sync/blob/main/.config/coverage.env).
Install those prerequisites before running a report:

```bash
rustup toolchain install 1.88.0 --component llvm-tools-preview
cargo +1.88.0 install cargo-llvm-cov --version 0.8.7 --locked
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

On 2026-08-26, the Bash `check` path passed with the pinned Rust 1.88.0 toolchain and
`cargo-llvm-cov 0.8.7`:

```bash
bash scripts/coverage.sh check
```

- Non-DSM lines: **18,972 / 20,299 = 93.4627%**
- DSM boundary lines: **7,396 / 9,978 = 74.1231%**
- Unfiltered total lines: **26,368 / 30,277 = 87.0892%**
- Tests: **489 passed** across all targets

This is the verified repository gate result; CI regenerates `coverage-summary.json` rather than
trusting a committed report.

For historical context, the pre-expansion audit used Rust 1.95 and the previously installed
`cargo-llvm-cov 0.6.11`; it measured **66.40% lines** (8,362 covered of 12,594 executable lines,
4,232 missed). That figure is retained only as the historical baseline, not the current pinned gate.

Coverage is evidence that code ran, not proof that the asserted behavior is correct. Do not lower
either threshold or exclude production files; retain the behavioral safety assertions and live-NAS
acceptance boundary.
