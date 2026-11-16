# Release selector

Choose a deployment target and this page will resolve it to one release asset. For a DSM
package, the decision checks three independent facts: the exact Synology model, the installed
Synology OS line/version, and the processor architecture reported by the NAS. An exact build is
required on DSM 7.0 to prove the package minimum and on DSM 7.4 to prove the package maximum when
you pick the branch yourself. It is optional on DSM 7.1–7.3, and the default **Automatic** branch
(see below) does not need it. DSM 7.4 must not exceed the SPK manifest maximum of `7.4-99999`. A
missing boundary build or mismatch stops the selector instead of guessing.

The 233-model physical catalog is a factual snapshot captured on **2026-08-26**. It contains all
231 records in Synology's
[CPU and Package Arch table](https://kb.synology.com/en-us/DSM/tutorial/What_kind_of_CPU_does_my_NAS_have)
plus `DVA7400` and `RS11626xs+`. Synology's official
[DSM 7.4.1-90080 PAT index](https://archive.synology.com/download/Os/DSM/7.4.1-90080) proves those two
physical models exist at that build, while its
[model-qualified package index](https://archive.synology.com/download/Package/SynoOnlinePack_v2/1071)
maps them to Package Arch `v1000nk` and `epyc7003`, respectively. The capture date is not a claim
about any source page's locale-dependent last-updated metadata. `VirtualDSM`, historical products
without current CPU/Package Arch evidence, and model-less toolkit targets are not guessed into the
physical catalog. For downloadable assets, the selector checks GitHub's public latest-release API
for an exact current file. If that lookup is unavailable, it shows the deterministic `YY.N`
filename pattern and links to Releases; it never constructs an unverified download. A container tag
is only correlated with the latest release and remains explicitly unverified until it is present in
GHCR; deploy its immutable digest after verification.

The model field is a single searchable combobox (an ARIA 1.2 combobox with a listbox) that works
with keyboard, mouse, and touch. It searches all 233 exact model names as you type. Matching ignores
case, spaces, and `+`, so `ds416`, `DS 416`, and `DS416` all find DS416, and `rs11626` finds
`RS11626xs+`. Every option shows the captured processor family, Synology Package Arch, and explicit
SPK asset correspondence. Models whose CPU family or product lifecycle has no published DSM 7
package say **manual review / no SPK**. A separate manual-review option preserves that same
fail-closed path for a model added after the snapshot.

Typing only filters the list. A model is selected only when you choose a listed entry (click, tap,
or the arrow keys plus Enter) or press Enter on text that is exactly one catalog model name. Free
text never resolves to a nearest-looking model, and editing the text after choosing a model clears
the choice until you choose again.

Choosing a model fills in the other two inputs, and you can override both:

- **Reported processor architecture** is auto-filled from the model's captured CPU and Package Arch
  record and the explicit Package Arch to SPK asset mapping (for example, DS416 is Package Arch
  `alpine4k`, so it fills `armv7` and the ARMv7 SPK). The field says where the value came from:
  Synology's CPU table, captured 2026-08-26. **Override** lets you enter what your NAS reports
  instead. A value that disagrees with the catalog still stops the selector. Models marked **manual
  review / no SPK**, and the not-listed option, fill nothing and keep the stop.
- **Installed OS branch** and **product line** default to **Automatic**. Automatic uses the newest
  DSM the model supports, from the lifecycle data in this page, and shows the resolved value (for
  example, "Automatic: DSM 7.4 for this model"). It needs no exact build when that DSM is 7.1 to
  7.4: the package accepts `7.0-40759` through `7.4-99999`, the model's newest DSM is above the
  minimum, and DSM build numbers have five digits, so every 7.4 build is within the maximum. So
  DS416 with Automatic resolves to the ARMv7 SPK with nothing typed. Only a model whose newest DSM
  were exactly 7.0 would still ask for the build, because the build decides whether `7.0-40759` is
  met. If you enter an exact build anyway, its branch is used and it is checked against the
  package range. A model whose last supported DSM is below 7.0, or a build from DSM 7.5 or later,
  stops with the same message an explicit pick gives. Automatic assumes the NAS runs the newest
  DSM its model supports; if yours is older (for example still on DSM 6.2), pick the branch by
  hand or enter the exact build. A manual pick of DSM 7.0 or 7.4 keeps the existing rule and
  still requires the exact build.

> **DSM package safety hold:** do not install `.spk` assets from releases `26.5`, `26.6`, or
> `26.20`. Release `26.20` rejects DSM's standard `system:system` authentication helper. The selector
> refuses these SPKs even when model, DSM, and architecture otherwise match.
> Release `26.5` contains identity-changing/set-ID privilege metadata that DSM rejects for a
> third-party package; release `26.6` instead requests the Synology-only `sysnotify` resource worker.
> The selector blocks those DSM assets even when GitHub reports one as the latest uploaded file and
> links to [GitHub Releases](https://github.com/supermarsx/synology-drive-sync/releases) instead of
> exposing a download. A non-blocked `26.7`-or-newer release is eligible only when its exact
> canonical asset is present. This hold does not block desktop, Rust SDK, C SDK, or container
> artifacts from the same tags because those artifacts are not DSM packages.

Release `26.10` introduced the native DSM AppWindow. Published `26.7`-`26.9` SPKs keep
the UI they were released with; select a non-blocked `26.10`-or-later release when the native
AppWindow is required, never `26.20`.
New SPKs declare `auto_upgrade_from="26.7-1"`: Package Center may offer a direct upgrade only from
the reviewed lifecycle-equivalent 26.7-26.10 generation or newer. Its only relevant source drift is
the fixed DSM notification application ID in 26.10; runner, lock, and lifecycle behavior is unchanged.
Installations on 26.5 or
26.6 must not be upgraded in place; use **Package Center > Manual Install** to move first to a
verified, non-blocked 26.7-or-later intermediate SPK, then install the current release. This
repository does not provide a private update feed or self-updater.

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
<option value="dsm-spk">Install the DSM dashboard package (SPK)</option>
<option value="desktop-cli">Run the desktop command-line tool</option>
<option value="rust-sdk">Integrate the Rust library</option>
<option value="c-abi">Integrate the C ABI / DLL / shared library</option>
<option value="container">Run the container image</option>
</select>
</div>
</div>
<fieldset data-dsm-fields>
<legend>DiskStation facts</legend>
<p class="selector-field-help" id="dsm-input-help">Pick the model, and the processor architecture and DSM version fill in for you. Use the values reported by this NAS to override either. The lookup does not replace the runtime check.</p>
<div class="selector-grid">
<div class="selector-field selector-field-wide">
<label for="synology-model">Exact Synology model</label>
<div class="model-combobox">
<input id="synology-model" class="model-input" type="text" role="combobox" aria-autocomplete="list" aria-expanded="false" aria-controls="synology-model-list" aria-describedby="model-catalog-status model-fact model-results" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" enterkeyhint="done" placeholder="Type a model, for example DS416 or RS1221+" data-model-input disabled>
<input type="hidden" name="model" value="" data-model-select>
<ul id="synology-model-list" class="model-listbox" role="listbox" aria-label="Synology models" data-model-listbox hidden></ul>
</div>
<small id="model-results" class="selector-visually-hidden" aria-live="polite" data-model-results></small>
<small id="model-catalog-status"><span data-model-count>Loading the exact model catalog…</span>. Search ignores case, spaces, and “+”, and every option shows its processor, Package Arch, and corresponding SPK asset or manual-review status. Only a listed model, or the manual-review option, can be selected. <noscript>Searching needs JavaScript; without it, use the lifecycle tables below.</noscript></small>
<small id="model-fact" data-model-fact aria-live="polite"></small>
</div>
<div class="selector-field">
<label for="product-line">Synology OS product line</label>
<select id="product-line" name="productLine">
<option value="auto" selected>Automatic (from the model)</option>
<option value="dsm">DiskStation Manager (DSM)</option>
<option value="dsm-enterprise">DSM Enterprise (informational; no published SPK)</option>
</select>
</div>
<div class="selector-field">
<label for="os-version">Installed OS branch</label>
<select id="os-version" name="osVersion" data-os-version aria-describedby="os-version-auto">
<option value="auto" selected>Automatic (newest for this model)</option>
<optgroup label="Supported DSM 7 release contract">
<option value="dsm-7.4">DSM 7.4</option>
<option value="dsm-7.3">DSM 7.3</option>
<option value="dsm-7.2">DSM 7.2</option>
<option value="dsm-7.1">DSM 7.1</option>
<option value="dsm-7.0">DSM 7.0</option>
</optgroup>
<optgroup label="Informational only — no compatible SPK">
<option value="dsm-6.x-or-earlier">DSM 6.x or earlier</option>
<option value="dsm-enterprise-1.0">DSM Enterprise 1.0</option>
</optgroup>
</select>
<small id="os-version-auto" data-os-auto aria-live="polite"></small>
</div>
<div class="selector-field">
<label for="dsm-version">Exact installed version/build</label>
<input id="dsm-version" name="dsmVersion" inputmode="text" placeholder="7.2.2-72806" data-dsm-version aria-describedby="dsm-version-help">
<small id="dsm-version-help">Required for DSM 7.0 and 7.4 to prove the package's minimum or maximum; optional for DSM 7.1–7.3. Use the complete version and build after the hyphen. With Automatic, the build is optional for DSM 7.1–7.4 and, when entered, sets the branch.</small>
</div>
<div class="selector-field">
<label for="reported-arch">Reported processor architecture</label>
<div class="arch-row">
<input id="reported-arch" name="reportedArch" inputmode="text" placeholder="armv7-a / armv7l" data-arch-input aria-describedby="reported-arch-help reported-arch-source">
<button type="button" class="selector-secondary" data-arch-override hidden>Override</button>
</div>
<small id="reported-arch-help">Recognized families: <code>x86_64</code>, <code>aarch64</code>/<code>armv8</code>, <code>armv7l</code>/<code>armv7-a</code>, and <code>i686</code>.</small>
<small id="reported-arch-source" data-arch-source aria-live="polite"></small>
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
<button type="submit"><svg class="selector-action-icon" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="square" stroke-linejoin="miter" aria-hidden="true" focusable="false"><path d="M8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2"></path><rect x="5" y="5" width="6" height="6"></rect><path d="M8 6.5v3M6.5 8h3"></path></svg> Find my release</button>
<span class="selector-status" role="status" aria-live="polite" data-selector-status>Ready to check.</span>
</div>
</form>
<section class="selector-result" aria-live="polite" data-selector-result hidden></section>
</div>

<noscript>
  <div class="selector-noscript">
    <strong>JavaScript is disabled.</strong> The static matrix below is an ABI inventory, not enough
    to select an SPK by itself. Cross-check the exact model in the linked Synology lifecycle,
    archive, and CPU tables, then verify that the exact filename exists in GitHub Releases.
  </div>
</noscript>

## Published target matrix

| Purpose | Supported target | Release form |
| --- | --- | --- |
| DSM dashboard package | `x86_64` family on a model/DSM branch accepted by the selector | `synology-drive-sync-YY.N-x86_64.spk` |
| DSM dashboard package | AArch64 / `armv8` family on a model/DSM branch accepted by the selector | `synology-drive-sync-YY.N-armv8.spk` |
| DSM dashboard package | ARMv7-A hard-float: Alpine, Armada 370/375/38x/XP, Comcerto2k, Monaco | `synology-drive-sync-YY.N-armv7.spk` |
| DSM dashboard package | Evansport `i686` on DSM 7.0–7.1 | `synology-drive-sync-YY.N-i686.spk` |
| Desktop CLI | Linux, macOS, Windows on x86-64 or ARM64 | `synology-drive-sync-YY.N-OS-ARCHIVE` |
| Rust library | Platform-neutral source bundle | `synology-drive-sync-YY.N-rust-sdk.tar.gz` |
| C ABI | Linux `.so`, macOS `.dylib`, or Windows DLL on x86-64/ARM64 | `synology-drive-sync-YY.N-c-sdk-OS-ARCHIVE` |
| Container | Linux amd64/arm64 OCI index | `ghcr.io/supermarsx/synology-drive-sync:YY.N` |

### DSM toolkit intervals

The selector has two independent compatibility layers. First, every one of the 233 models in the
physical catalog has an explicit OS product and model-specific DSM interval. Second, the model's
Package Arch must exist in Synology's toolkit branch. Passing only one layer is not enough.

Model lifecycle data was reconciled on **2026-08-26** against Synology's official
[last-upgradable software version table](https://kb.synology.com/en-us/DSM/tutorial/What_is_the_last_upgradable_software_version_for_my_Synology_product)
and the official model-specific PAT indexes for
[DSM 7.0](https://archive.synology.com/download/Os/DSM/7.0.1-42218),
[DSM 7.1](https://archive.synology.com/download/Os/DSM/7.1-42661-1),
[DSM 7.2](https://archive.synology.com/download/Os/DSM/7.2.2-72806),
[DSM 7.3](https://archive.synology.com/download/Os/DSM/7.3-81180), and
[DSM 7.4](https://archive.synology.com/download/Os/DSM/7.4.1-90080). The catalog partitions all
233 records exactly once:

| Product/model lifecycle | Models | Selector behavior |
| --- | ---: | --- |
| Last upgradable DSM 5.2 | 10 | Informational; no SPK |
| Last upgradable DSM 6.2 | 41 | Informational; no SPK |
| DSM 7.0–7.1 | 42 | Exact model and branch must agree |
| DSM 7.0–7.4 | 92 | Exact model and branch must agree |
| DSM 7.1–7.4 | 7 | Reject DSM 7.0 |
| DSM 7.2–7.4 | 22 | Reject DSM 7.0/7.1 |
| DSM 7.4 only | 18 | Reject every earlier branch; six require `7.4.1-90080` or later |
| DSM Enterprise 1.0 | 1 (`PAS7700`) | Informational; no ordinary DSM SPK |

Synology's lifecycle article intentionally omits models whose final version is not yet fixed and
directs users to Download Center. For those models, the catalog uses the earliest official DSM 7
branch that publishes that exact model and caps recommendations at the latest reviewed DSM 7.4
branch. Six physical models are absent from the `7.4-90075` PAT index and first appear at
`7.4.1-90080`: `DS1525neo+`, `DS1825neo+`, `DS725neo+`, `DS925neo+`, `DVA7400`, and
`RS11626xs+`. The PAT indexes establish this exact build boundary; the package index independently
establishes Package Arch `v1000nk` for `DVA7400` and `epyc7003` for `RS11626xs+`. The selector
therefore requires at least `7.4.1-90080` for all six and never backfills them onto an older image.
The four `neo+` records are also corroborated by Synology's official
[2026 product announcement](https://www.synology.com/ja-jp/company/news/article/dsneoplus). A future
model or DSM 7.5+ remains unknown until the source snapshot is refreshed.

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
DSM branch, and a removed platform is not guessed onto a newer one. Toolkit-only values without an
exact physical model in the provenance-complete catalog, including `kvmx64`, `kvmcloud`, and
`epyc7003ntb`, cannot bypass the exact-model check.

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

Search for the printed hardware model in the model field and choose it; the processor architecture
and DSM branch fill in automatically. Enter the exact product version plus build number (required if you pick DSM 7.0 or 7.4 yourself,
optional with Automatic and for DSM 7.1–7.3), and use **Override** on the architecture field only if
`uname -m` reports something different from the catalog. If Synology has added a model
after the snapshot date, consult the live
[official CPU table](https://kb.synology.com/en-us/DSM/tutorial/What_kind_of_CPU_does_my_NAS_have)
and open an issue with all four outputs. The selector intentionally has no "closest model" rule.

After downloading, follow [release verification](releases.md) before installing or linking an
artifact.
