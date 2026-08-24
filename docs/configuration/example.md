# Full configuration example

This is the exact commented starter shipped at the repository root. `config init` writes the same
content. Because the page uses an mdBook include, changes to the canonical example appear here on the
next documentation build instead of creating a second hand-maintained copy.

```toml
{{#include ../../config.example.toml}}
```

After editing a copy, validate and inspect the effective non-secret result:

```bash
synology-drive-sync config validate --config ./config.toml
synology-drive-sync config show --config ./config.toml --profile production
```

Return to the [complete TOML reference](reference.md) for types, defaults, cross-field constraints,
and the corresponding CLI/environment inputs.
