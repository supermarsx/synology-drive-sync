# Testing and coverage

The normal local gate mirrors the Rust portions of CI:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
```

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
`cargo llvm-cov --locked --workspace --all-targets`, and writes the machine-readable summary to
`target/llvm-cov/coverage-summary.json`. Coverage artifacts stay under ignored `target/` paths.

The CI policy is an unfiltered **90% total line-coverage minimum**. It measures all instrumented
lines in the Rust files under `src`, including inline `#[cfg(test)]` code; there is no filename
exclusion list. Doctests are not included because stable `cargo-llvm-cov` treats doctest coverage
as experimental.

## Verified pinned result

On 2026-08-05, the PowerShell `Check` path passed with the pinned Rust 1.88.0 toolchain and
`cargo-llvm-cov 0.8.7`:

```powershell
pwsh -File scripts/coverage.ps1 -Mode Check
```

- Lines: **14,136 / 15,600 = 90.6154%**
- Regions: **89.8962%**
- Functions: **87.9074%**
- Tests: **261 passed** across all targets

This is the verified repository gate result; CI regenerates `coverage-summary.json` rather than
trusting a committed report.

For historical context, the pre-expansion audit used Rust 1.95 and the previously installed
`cargo-llvm-cov 0.6.11`; it measured **66.40% lines** (8,362 covered of 12,594 executable lines,
4,232 missed). That figure is retained only as the historical baseline, not the current pinned gate.

Coverage is evidence that code ran, not proof that the asserted behavior is correct. Do not lower
the threshold or exclude production files; retain the behavioral safety assertions and live-NAS
acceptance boundary.
