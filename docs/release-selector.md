# Release selector

Choose a deployment target and this page will resolve it to one release asset. For a DSM
package, the decision checks three independent facts: the exact Synology model, the installed
DSM build, and the processor architecture reported by the NAS. A mismatch stops the selector
instead of guessing.

The 231-model catalog is a factual snapshot of Synology's
[CPU and Package Arch table](https://kb.synology.com/en-us/DSM/tutorial/What_kind_of_CPU_does_my_NAS_have),
captured on **2026-08-24**. That is this catalog's extraction date, not a claim about the source
page's locale-dependent last-updated metadata. For downloadable assets, the selector checks GitHub's
public latest-release API for an exact current file. If that lookup is unavailable, it shows the
deterministic `YY.N` filename pattern and links to Releases; it never constructs an unverified
download. A container tag is only correlated with the latest release and remains explicitly
unverified until it is present in GHCR; deploy its immutable digest after verification.

DSM compatibility follows SynologyOpenSource's
[`pkgscripts-ng` platform mapping through DSM 7.4](https://github.com/SynologyOpenSource/pkgscripts-ng/blob/DSM7.4/include/pkg_util.sh).
DSM 7.5 or a later major release therefore stops without a recommendation until this snapshot is
reviewed and updated.

<div class="release-selector" data-release-selector>
<form class="selector-form">
<div class="selector-grid selector-grid-purpose">
<div class="selector-field">
<label for="release-purpose">What do you need?</label>
<select id="release-purpose" name="purpose">
<option value="dsm-spk">Install directly in DSM Package Center</option>
<option value="desktop-cli">Run the desktop command-line tool</option>
<option value="rust-sdk">Integrate the Rust library</option>
<option value="c-abi">Integrate the C ABI / DLL / shared library</option>
<option value="container">Run the container image</option>
</select>
</div>
</div>
<fieldset data-dsm-fields>
<legend>DiskStation facts</legend>
<p class="selector-field-help" id="dsm-input-help">Use the values reported by this NAS. The model lookup does not replace the runtime check.</p>
<div class="selector-grid">
<div class="selector-field">
<label for="synology-model">Exact Synology model</label>
<input id="synology-model" name="model" list="synology-models" autocomplete="off" placeholder="DS419slim" aria-describedby="model-fact">
<datalist id="synology-models" data-model-list></datalist>
<small id="model-fact" data-model-fact aria-live="polite"></small>
</div>
<div class="selector-field">
<label for="dsm-version">DSM version and build</label>
<input id="dsm-version" name="dsmVersion" inputmode="text" placeholder="7.2.2-72806" aria-describedby="dsm-version-help">
<small id="dsm-version-help">Include the build after the hyphen.</small>
</div>
<div class="selector-field selector-field-wide">
<label for="reported-arch">Reported processor architecture</label>
<input id="reported-arch" name="reportedArch" inputmode="text" placeholder="armv7-a / armv7l" aria-describedby="reported-arch-help">
<small id="reported-arch-help">Recognized families: <code>x86_64</code>, <code>aarch64</code>/<code>armv8</code>, <code>armv7l</code>/<code>armv7-a</code>, and <code>i686</code>.</small>
</div>
</div>
</fieldset>
<fieldset data-desktop-fields>
<legend>Desktop or build host</legend>
<div class="selector-grid">
<div class="selector-field">
<label for="desktop-os">Operating system</label>
<select id="desktop-os" name="desktopOs">
<option value="linux">Linux</option>
<option value="macos">macOS</option>
<option value="windows">Windows</option>
</select>
</div>
<div class="selector-field">
<label for="desktop-cpu">Processor</label>
<select id="desktop-cpu" name="desktopCpu">
<option value="x86_64">x86-64 / AMD64</option>
<option value="aarch64">ARM64 / AArch64 / Apple silicon</option>
<option value="i686">32-bit x86 (no published desktop asset)</option>
<option value="armv7">32-bit ARMv7 (DSM SPK only)</option>
</select>
</div>
</div>
</fieldset>
<div class="selector-actions">
<button type="submit">Find my release</button>
<span class="selector-status" role="status" aria-live="polite" data-selector-status>Ready to check.</span>
</div>
</form>
<section class="selector-result" aria-live="polite" data-selector-result hidden></section>
</div>

<noscript>
  <div class="selector-noscript">
    <strong>JavaScript is disabled.</strong> Use the static matrix below, replace <code>YY.N</code>
    with the current release tag, and verify the filename in GitHub Releases before installing it.
  </div>
</noscript>

## Published target matrix

| Purpose | Supported target | Release form |
| --- | --- | --- |
| DSM Package Center | `x86_64` family | `synology-drive-sync-YY.N-x86_64.spk` |
| DSM Package Center | AArch64 / `armv8` family | `synology-drive-sync-YY.N-armv8.spk` |
| DSM Package Center | ARMv7-A hard-float: Alpine, Armada 370/375/38x/XP, Comcerto2k, Monaco | `synology-drive-sync-YY.N-armv7.spk` |
| DSM Package Center | Evansport `i686` on DSM 7.0–7.1 | `synology-drive-sync-YY.N-i686.spk` |
| Desktop CLI | Linux, macOS, Windows on x86-64 or ARM64 | `synology-drive-sync-YY.N-OS-ARCHIVE` |
| Rust library | Platform-neutral source bundle | `synology-drive-sync-YY.N-rust-sdk.tar.gz` |
| C ABI | Linux `.so`, macOS `.dylib`, or Windows DLL on x86-64/ARM64 | `synology-drive-sync-YY.N-c-sdk-OS-ARCHIVE` |
| Container | Linux amd64/arm64 OCI index | `ghcr.io/supermarsx/synology-drive-sync:YY.N` |

### DSM toolkit intervals

The selector requires the model's official Package Arch to appear in the entered DSM minor branch.
The intervals below are derived from `AllPlatformOptionNames` in SynologyOpenSource's official
[`DSM7.0`](https://github.com/SynologyOpenSource/pkgscripts-ng/blob/DSM7.0/include/platforms),
[`DSM7.1`](https://github.com/SynologyOpenSource/pkgscripts-ng/blob/DSM7.1/include/platforms),
[`DSM7.2`](https://github.com/SynologyOpenSource/pkgscripts-ng/blob/DSM7.2/include/platforms),
[`DSM7.3`](https://github.com/SynologyOpenSource/pkgscripts-ng/blob/DSM7.3/include/platforms), and
[`DSM7.4`](https://github.com/SynologyOpenSource/pkgscripts-ng/blob/DSM7.4/include/platforms)
branches:

| Verified DSM interval | Official model Package Arch values |
| --- | --- |
| 7.0–7.1 | `armada370`, `armada375`, `armadaxp`, `cedarview`, `comcerto2k`, `evansport` |
| 7.0–7.4 | `alpine`, `alpine4k`, `apollolake`, `armada37xx`, `armada38x`, `avoton`, `braswell`, `broadwell`, `broadwellnk`, `bromolow`, `denverton`, `geminilake`, `grantley`, `monaco`, `purley`, `rtd1296`, `v1000` |
| 7.1–7.4 | `broadwellnkv2`, `epyc7002`, `r1000`, `rtd1619b` |
| 7.2–7.4 | `broadwellntbap`, `geminilakenk`, `r1000nk`, `v1000nk` |
| 7.3–7.4 | `epyc7003`, `icelaked` |

This enforces both ends of the interval: a newly introduced platform is not guessed onto an older
DSM branch, and a removed platform is not guessed onto a newer one. Toolkit-only values without a
model in the captured CPU table, including `kvmx64`, `kvmcloud`, and `epyc7003ntb`, cannot bypass the
exact-model check.

`PAS7700` deliberately remains in the factual CPU/Package Arch snapshot, but it never produces a
DSM SPK recommendation. Synology's official [PAS7700 Download Center](https://www.synology.com/en-us/support/download/PAS7700)
and product guidance identify its operating system as **DSM Enterprise 1.0**, whose package lifecycle
is outside this DSM 7 release contract. Sharing the `epyc7003` CPU-table Package Arch does not make a
DSM 7 SPK compatible.

The ARMv7 SPK contains one static, little-endian ARMv7-A EABI5 hard-float executable. Its
`INFO` architecture list covers `armv7` (Alpine and Alpine4k) plus the platform-specific
`armada370`, `armada375`, `armada38x`, `armadaxp`, `comcerto2k`, and `monaco` values. It does
not relabel an ARM64 binary.

ARMv5/88f628x, PowerPC, QorIQ, old `x86` Package Arch, unknown models, and contradictory
model/runtime inputs do not produce a recommendation. Those systems are outside the verified DSM
7 native matrix. A safe legacy port would require its own package lifecycle, testing, and release;
changing only `arch` would bypass DSM's safety gate without making the binary compatible.

## Collect the NAS values

SSH to the DiskStation and capture these read-only values:

```sh
uname -m
cat /proc/sys/kernel/syno_hw_version
get_key_value /etc.defaults/synoinfo.conf unique
grep -E '^(majorversion|minorversion|productversion|buildnumber|smallfixnumber)=' /etc.defaults/VERSION
```

Use the printed hardware model in the model field, the DSM product version plus build number in the
version field, and `uname -m` in the reported-architecture field. If Synology has added a model
after the snapshot date, consult the live
[official CPU table](https://kb.synology.com/en-us/DSM/tutorial/What_kind_of_CPU_does_my_NAS_have)
and open an issue with all four outputs. The selector intentionally has no "closest model" rule.

After downloading, follow [release verification](releases.md) before installing or linking an
artifact.
