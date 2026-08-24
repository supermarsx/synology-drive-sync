# Rust SDK example

Calendar releases identify audited repository snapshots. Until the Rust crate
adopts a calendar-compatible semantic package version, depend on an exact Git
tag instead of pretending the package's `0.1.0` version changes on every push:

```toml
[dependencies]
synology-drive-sync = {
  git = "https://github.com/supermarsx/synology-drive-sync",
  tag = "26.1"
}
```

Build the example:

```bash
cargo build --locked --example sdk-basic
```

It previews by default. Passing `--apply` is the explicit plan decision that
permits mutation:

```bash
cargo run --locked --example sdk-basic -- \
  https://files.example.invalid user ./source /home/Drive/backup --apply
```

The password is requested only after local scanning and File Station API
discovery. The OTP prompt is reached only after DSM issues an OTP challenge.
