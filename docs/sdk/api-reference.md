# Generated Rust API

The documentation workflow runs `cargo doc --locked --no-deps --lib` against the exact source commit
and publishes the result beside this book.

[Open the generated `synology_drive_sync` API reference](../api/synology_drive_sync/index.html)

> [!NOTE]
> Generated rustdoc describes signatures and type-level documentation. The safety sequence,
> operational limits, and caller responsibilities in this guide remain normative for integration.

If that link is missing in a local checkout, run:

```bash
cargo doc --locked --no-deps --lib
mdbook build
```

Then copy the contents of `target/doc/` into `target/site/api/`, matching the Pages workflow.
