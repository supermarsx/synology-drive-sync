# Contributing

The authoritative contributor workflow lives in
[CONTRIBUTING.md](https://github.com/supermarsx/synology-drive-sync/blob/main/CONTRIBUTING.md).

Before proposing a change:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-targets
cargo build --release --locked -p synology-drive-sync
cargo build --profile ffi-release --locked -p synology-drive-sync-ffi
mdbook test
mdbook build
```

The C ABI must use the dedicated unwind-safe `ffi-release` profile; the ordinary release profile is for the CLI.

Run the focused package/service tests for any affected deployment surface and keep generated
third-party notices synchronized with the locked graph. See [testing and coverage](testing.md) for
the repository's test layers and coverage gate.

Documentation changes should preserve existing public flat-page paths, add every navigable chapter
to `SUMMARY.md`, use relative internal links inside `docs/`, and pass the generated-site link checker.
Never put real endpoints, usernames, paths containing personal data, or secrets into examples.
