# Contributing

Bug reports and focused pull requests are welcome. For behavioral changes, open an issue first so the safety and File Station compatibility tradeoffs can be agreed on.

Before submitting a pull request, run:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
bash .github/scripts/run-cargo-audit.sh
bash .github/scripts/generate-third-party-notices.sh
git diff --exit-code -- third_party_licenses.html
```

Run the reproducible full-source coverage report described in
[Testing and coverage](docs/testing.md). The CI coverage job preserves the unfiltered report while
enforcing a 90% non-DSM line floor and a 74% floor across the exact DSM bridge boundary, using the
pinned toolchain and `cargo-llvm-cov` version.

The two supply-chain helpers use SHA-256-pinned official tool archives. The RustSec helper updates
the advisory database before checking `Cargo.lock`; the notice helper fetches checksum-locked
crates for all release targets and then generates offline. Commit a regenerated
`third_party_licenses.html` whenever the locked dependency graph changes.

Never commit DSM passwords, TOTP seeds, `otpauth` provisioning URIs, one-time codes, session IDs, SynoTokens, private endpoint names, or captures containing file data. New remote-mutation behavior must include failure-ordering and delete-safety tests.
