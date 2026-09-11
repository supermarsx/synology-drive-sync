# AppWindow bundle budget

The DSM AppWindow ships as exactly two built artifacts, `dist/SynologyDriveSync.js` and
`dist/style.css`. `validate_spk.py` pins that to two files and pins `config.define` to the single
JavaScript module, so code splitting is not available: `splitChunks: false` and `runtimeChunk: false`
are themselves pinned literals in the webpack contract.

The budget is `maxAssetSize` / `maxEntrypointSize` at 512,000 bytes in `webpack.config.js`. It is
declared with `hints: "warning"`, so exceeding it prints a warning rather than failing the build.
Treat it as binding anyway — it is the only stated ceiling on what DSM has to parse before the
AppWindow paints.

## A quarter of the JavaScript bundle is stylesheet

`_validate_runtime_style_bundle` requires the executed bundle to embed the exact bytes of the
packaged stylesheet, so that an upgraded package cannot run current markup against a stale cached
CSS response for the same package URL. `src/runtimeStyles.js` installs those bytes at runtime.

The consequence is easy to miss: **`dist/style.css` is embedded whole, as a single JavaScript string
literal, inside `dist/SynologyDriveSync.js`.** `src/styles/native.css` is pretty-printed — two-space
indented, one declaration per line — and `build/condense-css-loader.js` strips its comments,
indentation and blank lines on the way into both artifacts, so the two are no longer byte-identical.
That leaves 122,014 bytes of a 531,631-byte "JavaScript" bundle; before the condenser it was 132,891
of 533,741.

Every byte trimmed from the stylesheet is a byte off the JavaScript asset as well — but the
stylesheet is now spent as a lever, and the numbers say so:

- condensing saves 11,008 bytes of CSS and 11,735 of bundle, and is the whole of what is available
  without loosening a contract;
- a real minifier tops out near 23 KB of CSS at maximum aggression, and cannot be used: csso,
  cssnano and clean-css each break eleven byte-level checks in `test/*.test.mjs` and
  `validate_spk.py` — ` > ` spacing, the space before `!important`, at-rule prelude spacing,
  `from`/`to`, `minmax(112px, 0.72fr)`, newline-anchored selectors, and the 260/220-char keyframe
  windows. No minifier has a knob for any of them.

The bundle exceeds the 512,000-byte hint by about 19.6 KB. It was about 10.0 KB over before queued
progress landed, and about 21.7 KB over before the stylesheet was condensed. Any further reduction
has to come out of the JavaScript: at ~410 KB excluding the embedded stylesheet, that is where the
remaining headroom is, and there is no second stylesheet-shaped win behind it.

Four features have landed over the budget since it was first written (the timestamped log view and
per-category clearing, the stored-totals summary that made the Sync section open without a walk, and
then queued-job progress). Each was worth its bytes; none of them is where the headroom is.

### What queued progress cost, and what that 9,625 bytes is made of

Measured by building with each block removed in turn:

| Block | Bundle bytes |
| --- | --- |
| `PHASE_SPECS`, the cross-layer phase catalogue mirrored from `src/lib.rs` | 1,962 |
| `QUEUED_TERMINAL_FAILURE_CODES` and `QUEUED_FAILURE_COPY`, the vocabulary and the cause and next step for each named terminal failure | 1,209 |
| Everything else | 6,454 |

That remainder is the part with no single owner: the six-key validator and its enumeration in
`api.js`, the render-boundary guard and phase/count/staleness formatting, the controller-liveness
join and its six branches of copy, the progress sink threaded through the result poll,
`QueuedConsumerFailedError`, three template live regions, and about 131 bytes of new stylesheet
counted twice because the stylesheet is embedded in the bundle.

Two of these three are prose, and prose is the part a minifier cannot touch: it is shipped verbatim
because it is the feature. The catalogue is the only block with a structural alternative — packing
each phase as an array rather than a frozen object would recover roughly 400 bytes — and it is
deliberately not taken, because the object form is what `test_synology_ui.py` greps to hold the
mirror in step with `src/lib.rs`, and a silently skewed catalogue under-reports a slow job at exactly
the moment someone is troubleshooting it.

## Constraints on transforming the stylesheet

These three shaped the condense loader, and still bind anything further.

1. **Both consumers must receive the same bytes.** The stylesheet is loaded twice by different
   rules in `webpack.config.js`: `resourceQuery: /runtime/` reads the source through
   `type: "asset/source"`, while the extracted `dist/style.css` goes through `css-loader` and
   `MiniCssExtractPlugin`. Transforming only the extract path makes them diverge, and
   `_validate_runtime_style_bundle` fails with "does not embed the exact packaged stylesheet
   bytes" — a message that names the symptom rather than the fork that caused it.
   `condense-css-loader` runs as an `enforce: "pre"` loader, upstream of that fork, so both
   branches start from its output; a pre-loader cannot be bypassed by editing the `oneOf`, and the
   two import forms in `src/main.js` are themselves pinned literals, which closes the `!!` inline
   route. Nothing in webpack's rule model stops a second transform being added to one branch's
   `use` array later, so `AssertRuntimeStyleParityPlugin` checks the two still agree at
   `PROCESS_ASSETS_STAGE_REPORT` and fails the build naming the fork.

2. **A new devDependency changes the lockfile.** `validate_ui_dependency_resolution` reads both
   `pnpm-workspace.yaml` and `pnpm-lock.yaml`, and CI runs `pnpm audit --audit-level high`. The
   condenser adds no dependency, so none of that moved. The licence gate is not the obstacle it
   looks like, though: `appwindow-help-security.test.mjs` enumerates only the packages whose code
   lands in the bundle, and `about.toml` is cargo-about, so a build-only minimizer would need no
   notice entry. What rules a minimizer out is the byte-level contracts above, not its licence.

3. **`NormalizeCssNewlinePlugin` still owns the trailing newline.** It rewrites the `style.css`
   asset at `PROCESS_ASSETS_STAGE_SUMMARIZE` to end in exactly one `\n`. Any transform must run
   before that stage — `condense-css-loader` runs at loader time — and any check of the emitted
   asset must run after it, which is why the parity plugin taps `PROCESS_ASSETS_STAGE_REPORT`.

## Measuring

Measure at the byte level, never through `git status` or `git diff` — both normalize line endings on
read, and this repository has lost hours to that:

```bash
python -c "import os; print(os.path.getsize('packaging/synology/ui-src/dist/SynologyDriveSync.js'))"
python -c "print(open('packaging/synology/ui-src/dist/SynologyDriveSync.js','rb').read().count(b'\r'))"
```

`dist/` is committed and CI asserts it rebuilds reproducibly (`git diff --exit-code -- dist`), so
rebuild and commit it with any change that affects it.

The build is reproducible across Node patch releases, not only under the pinned one: the committed
`dist/` has been rebuilt byte-for-byte under both CI's pinned 24.20.0 and a local 24.14.1. A local
rebuild that differs is therefore a real signal about the change, not about the Node version.
