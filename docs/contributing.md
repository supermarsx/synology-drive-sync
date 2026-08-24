# Contributing

The authoritative contributor workflow lives in
[CONTRIBUTING.md](https://github.com/supermarsx/synology-drive-sync/blob/main/CONTRIBUTING.md).

Before proposing a change:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo build --release --locked
mdbook test
mdbook build
```

Run the focused package/service tests for any affected deployment surface and keep generated
third-party notices synchronized with the locked graph. See [testing and coverage](testing.md) for
the repository's test layers and coverage gate.

Documentation changes should preserve existing public flat-page paths, add every navigable chapter
to `SUMMARY.md`, use relative internal links inside `docs/`, and pass the generated-site link checker.
Never put real endpoints, usernames, paths containing personal data, or secrets into examples.
