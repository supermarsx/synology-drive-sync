# AppWindow bundle budget

The DSM AppWindow ships as exactly two built artifacts, `dist/SynologyDriveSync.js` and
`dist/style.css`. `validate_spk.py` pins that to two files and pins `config.define` to the single
JavaScript module, so code splitting is not available: `splitChunks: false` and `runtimeChunk: false`
are themselves pinned literals in the webpack contract.

The budget is `maxAssetSize` / `maxEntrypointSize` at 512,000 bytes in `webpack.config.js`. It is
declared with `hints: "warning"`, so exceeding it prints a warning rather than failing the build.
Treat it as binding anyway — it is the only stated ceiling on what DSM has to parse before the
AppWindow paints.

## A quarter of the JavaScript bundle is unminified CSS

`_validate_runtime_style_bundle` requires the executed bundle to embed the exact bytes of the
packaged stylesheet, so that an upgraded package cannot run current markup against a stale cached
CSS response for the same package URL. `src/runtimeStyles.js` installs those bytes at runtime.

The consequence is easy to miss: **`dist/style.css` is embedded whole, as a single JavaScript string
literal, inside `dist/SynologyDriveSync.js`.** There is no CSS minimizer anywhere in the pipeline, so
`dist/style.css` is byte-identical to `src/styles/native.css` — pretty-printed, two-space indented,
one declaration per line. At the time of writing that is 131,489 bytes of a 520,634-byte "JavaScript"
bundle.

Every byte trimmed from the stylesheet is therefore a byte off the JavaScript asset as well.
Minifying it is worth roughly 30 KB against a budget the bundle currently exceeds by single-digit
kilobytes — an order of magnitude more headroom than any plausible amount of JavaScript golfing.

## Constraints on doing it

Minification is not a one-line webpack change. Three things have to hold.

1. **Both consumers must receive the same bytes.** The stylesheet is loaded twice by different
   rules in `webpack.config.js`: `resourceQuery: /runtime/` reads the raw source through
   `type: "asset/source"`, while the extracted `dist/style.css` goes through `css-loader` and
   `MiniCssExtractPlugin`. Today those agree because nothing transforms either path. Minifying only
   the extract path makes them diverge, and `_validate_runtime_style_bundle` fails with
   "does not embed the exact packaged stylesheet bytes". The minification has to happen upstream of
   the fork, or be applied identically to both.

2. **A new devDependency changes the lockfile.** `validate_ui_dependency_resolution` reads both
   `pnpm-workspace.yaml` and `pnpm-lock.yaml`, and the package's dependency set is reviewed. Adding
   a minimizer is a dependency-graph change, not a build tweak. CI runs
   `pnpm audit --audit-level high`, so check the candidate against it before adopting one.

3. **`NormalizeCssNewlinePlugin` still owns the trailing newline.** It rewrites the `style.css`
   asset at `PROCESS_ASSETS_STAGE_SUMMARIZE` to end in exactly one `\n`. Any minimizer must run
   before that stage, or the two will fight over the final byte.

## Measuring

Measure at the byte level, never through `git status` or `git diff` — both normalize line endings on
read, and this repository has lost hours to that:

```bash
python -c "import os; print(os.path.getsize('packaging/synology/ui-src/dist/SynologyDriveSync.js'))"
python -c "print(open('packaging/synology/ui-src/dist/SynologyDriveSync.js','rb').read().count(b'\r'))"
```

`dist/` is committed and CI asserts it rebuilds reproducibly (`git diff --exit-code -- dist`), so
rebuild and commit it with any change that affects it.
