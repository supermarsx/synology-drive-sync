# Rust SDK example

Calendar releases identify audited repository snapshots, and each one is published to crates.io
as crate version `0.YY.N` (release 26.66 is `0.26.66`). Depend on an exact version, because Cargo
treats every `0.26.x` as compatible:

```toml
[dependencies]
synology-drive-sync = "=0.26.66"
```

To build from an unpublished commit, depend on the Git tag instead:

```toml
[dependencies]
synology-drive-sync = {
  git = "https://github.com/supermarsx/synology-drive-sync",
  tag = "26.66"
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
