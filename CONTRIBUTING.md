# Contributing

Bug reports and focused pull requests are welcome. For behavioral changes, open an issue first so the safety and File Station compatibility tradeoffs can be agreed on.

Before submitting a pull request, run:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
```

Never commit DSM credentials, OTPs, session IDs, SynoTokens, private endpoint names, or captures containing file data. New remote-mutation behavior must include failure-ordering and delete-safety tests.
