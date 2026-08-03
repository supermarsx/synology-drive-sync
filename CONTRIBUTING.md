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
git diff --exit-code -- THIRD_PARTY_LICENSES.html
```

The two supply-chain helpers use SHA-256-pinned official tool archives. The RustSec helper updates
the advisory database before checking `Cargo.lock`; the notice helper fetches checksum-locked
crates for all release targets and then generates offline. Commit a regenerated
`THIRD_PARTY_LICENSES.html` whenever the locked dependency graph changes.

Never commit DSM passwords, TOTP seeds, `otpauth` provisioning URIs, one-time codes, session IDs, SynoTokens, private endpoint names, or captures containing file data. New remote-mutation behavior must include failure-ordering and delete-safety tests.
