import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";

const css = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");
const controlLayout = await readFile(new URL("../src/controlLayout.js", import.meta.url), "utf8");
const physicalControlFixture = await readFile(
  new URL("./fixtures/dsm-physical-control-dom.html", import.meta.url),
  "utf8"
);
const baselineCss = css.slice(0, css.indexOf("@container (max-width: 420px)"));
const inlineControlLayout = controlLayout.replace(/^export\s+/gm, "");

function chromeCandidates() {
  const configured = [process.env.CHROME_BIN, process.env.GOOGLE_CHROME_BIN].filter(Boolean);
  if (process.platform === "win32") {
    return configured.concat([
      "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
      "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
      "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
      "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe"
    ]);
  }
  if (process.platform === "darwin") {
    return configured.concat([
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
      "google-chrome",
      "chromium"
    ]);
  }
  return configured.concat(["google-chrome-stable", "google-chrome", "chromium", "chromium-browser"]);
}

function findChrome() {
  for (const candidate of chromeCandidates()) {
    if (candidate.includes("/") || candidate.includes("\\")) {
      if (existsSync(candidate)) return candidate;
      continue;
    }
    const probe = spawnSync(candidate, ["--version"], {
      encoding: "utf8",
      timeout: 5000,
      windowsHide: true
    });
    if (!probe.error && probe.status === 0) return candidate;
  }
  return null;
}

function overlapsVertically(a, b) {
  return Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top) > 4;
}

function decodeLayout(stdout) {
  const match = stdout.match(/data-layout="([A-Za-z0-9+/=]+)"/);
  assert.ok(match, `headless browser did not expose computed layout:\n${stdout.slice(-2000)}`);
  return JSON.parse(Buffer.from(match[1], "base64").toString("utf8"));
}

function terminateOwnedBrowserProcesses(profileDirectory) {
  if (!profileDirectory) return;
  if (process.platform === "win32") {
    spawnSync("powershell.exe", [
      "-NoProfile",
      "-NonInteractive",
      "-Command",
      "$needle = $env:SDSYNC_TEST_BROWSER_PROFILE; Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($needle) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }"
    ], {
      env: { ...process.env, SDSYNC_TEST_BROWSER_PROFILE: profileDirectory },
      encoding: "utf8",
      timeout: 5000,
      windowsHide: true
    });
    return;
  }
  spawnSync("pkill", ["-TERM", "-f", profileDirectory], {
    encoding: "utf8",
    timeout: 5000,
    windowsHide: true
  });
}

function renderAttempt(chrome, url, profileDirectory, timeoutMs = 45000) {
  const result = spawnSync(chrome, [
    "--headless=new",
    "--disable-gpu",
    "--disable-dev-shm-usage",
    "--disable-background-networking",
    "--disable-breakpad",
    "--disable-component-update",
    "--disable-crash-reporter",
    "--no-sandbox",
    `--user-data-dir=${profileDirectory}`,
    "--window-size=1100,1000",
    "--virtual-time-budget=500",
    "--dump-dom",
    url
  ], {
    encoding: "utf8",
    maxBuffer: 8 * 1024 * 1024,
    timeout: timeoutMs,
    windowsHide: true
  });
  terminateOwnedBrowserProcesses(profileDirectory);
  return result;
}

function renderAttemptSummary(result) {
  return {
    status: result.status,
    signal: result.signal || null,
    error: result.error ? String(result.error.code || result.error.message || result.error) : null,
    stdoutBytes: Buffer.byteLength(result.stdout || "", "utf8"),
    stderrTail: String(result.stderr || "").slice(-1200)
  };
}

function shouldRetryRender(result) {
  const timedOut = result.error && result.error.code === "ETIMEDOUT";
  const missingLayout = !/data-layout="[A-Za-z0-9+/=]+"/.test(result.stdout || "");
  return Boolean(timedOut || missingLayout);
}

function render(chrome, url, profileDirectory, { retry = true, timeoutMs = 45000 } = {}) {
  const attempts = [renderAttempt(chrome, url, profileDirectory, timeoutMs)];
  if (retry && shouldRetryRender(attempts[0])) {
    attempts.push(renderAttempt(chrome, url, `${profileDirectory}-retry`, timeoutMs));
  }
  const result = attempts[attempts.length - 1];
  const diagnostics = JSON.stringify(attempts.map(renderAttemptSummary));
  assert.equal(result.status, 0, `headless browser failed after ${attempts.length} bounded attempt(s): ${diagnostics}`);
  assert.match(result.stdout || "", /data-layout="[A-Za-z0-9+/=]+"/,
    `headless browser returned no computed layout after ${attempts.length} bounded attempt(s): ${diagnostics}`);
  return decodeLayout(result.stdout);
}

function browserProbePolicy({ chromeAvailable, ci, platform, arch, gate = "" }) {
  if (!["", "required", "skip-synology-architecture-matrix"].includes(gate)) {
    throw new Error(`unsupported SDSYNC_UI_BROWSER_GATE value: ${gate}`);
  }
  if (gate === "skip-synology-architecture-matrix" && ci) {
    return {
      mandatory: false,
      skipReason: "Reviewed skip: computed layout is authoritative in the general x64 packaging job"
    };
  }
  if (gate === "required") return { mandatory: true, skipReason: "" };
  if (chromeAvailable) return { mandatory: true, skipReason: "" };
  if (!ci) {
    return {
      mandatory: false,
      skipReason: "Chrome/Chromium is not installed for computed-style verification"
    };
  }
  if (platform === "linux" && arch === "arm64") {
    return {
      mandatory: false,
      skipReason: "Linux ARM64 CI image does not provide a supported Chrome/Chromium binary"
    };
  }
  return { mandatory: true, skipReason: "" };
}

const chrome = findChrome();
const runningInCi = /^(?:1|true)$/i.test(process.env.CI || "");
const browserPolicy = browserProbePolicy({
  chromeAvailable: Boolean(chrome),
  ci: runningInCi,
  platform: process.platform,
  arch: process.arch,
  gate: process.env.SDSYNC_UI_BROWSER_GATE || ""
});

test("computed-layout browser gate has one authoritative CI lane and reviewed matrix skips", () => {
  assert.equal(browserProbePolicy({
    chromeAvailable: true,
    ci: true,
    platform: "linux",
    arch: "x64",
    gate: "skip-synology-architecture-matrix"
  }).mandatory, false, "Synology architecture jobs skip the redundant probe even when Chrome is present");
  assert.match(browserProbePolicy({
    chromeAvailable: false,
    ci: true,
    platform: "linux",
    arch: "arm64",
    gate: "skip-synology-architecture-matrix"
  }).skipReason, /^Reviewed skip:/, "the ARM64 matrix reports the reviewed redundancy, not a false browser pass");
  assert.equal(browserProbePolicy({
    chromeAvailable: false,
    ci: true,
    platform: "linux",
    arch: "x64",
    gate: "required"
  }).mandatory, true, "the general x64 packaging gate must fail if its browser disappears");
  assert.equal(browserProbePolicy({
    chromeAvailable: false,
    ci: true,
    platform: "linux",
    arch: "arm64"
  }).mandatory, false, "Linux ARM64 CI without Chrome is the unsupported hosted runner");
  assert.equal(browserProbePolicy({
    chromeAvailable: false,
    ci: true,
    platform: "linux",
    arch: "x64"
  }).mandatory, true, "Linux x64 CI must fail if its browser disappears");
  assert.equal(browserProbePolicy({
    chromeAvailable: false,
    ci: true,
    platform: "win32",
    arch: "arm64"
  }).mandatory, true, "the CI waiver is not a general ARM64 waiver");
  assert.equal(browserProbePolicy({
    chromeAvailable: true,
    ci: true,
    platform: "linux",
    arch: "arm64"
  }).mandatory, true, "an available ARM64 browser must still run the computed-layout probe");
  assert.equal(browserProbePolicy({
    chromeAvailable: false,
    ci: false,
    platform: "linux",
    arch: "x64"
  }).mandatory, false, "local development without Chrome retains the existing explicit skip");
  assert.throws(
    () => browserProbePolicy({ chromeAvailable: true, ci: true, platform: "linux", arch: "x64", gate: "skip" }),
    /unsupported SDSYNC_UI_BROWSER_GATE/,
    "an unreviewed workflow value cannot silently disable the browser gate"
  );
  assert.equal(shouldRetryRender({ error: { code: "ETIMEDOUT" }, stdout: "" }), true);
  assert.equal(shouldRetryRender({ error: null, stdout: "" }), true);
  assert.equal(shouldRetryRender({ error: null, stdout: 'data-layout="e30="' }), false);
});

test("Chrome 88 fallback contains hostile DSM wrappers without modern CSS selectors", {
  skip: browserPolicy.skipReason || false
}, async () => {
  assert.ok(chrome, `Chrome/Chromium is required on ${process.platform}/${process.arch} CI`);
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "sdsync-control-layout-"));
  try {
    const hostileCss = `
      html, body { margin: 0; }
      #window-shell { margin-inline: auto; }
      .dsm-host .dsm-form-item {
        display: flex !important;
        min-width: 900px !important;
        flex-direction: column !important;
        gap: 32px !important;
        margin: 40px !important;
        padding: 30px !important;
      }
      .dsm-host .dsm-form-item > * {
        width: auto !important;
        max-width: none !important;
        min-width: 280px !important;
        margin: 24px !important;
        padding: 18px !important;
      }
      .dsm-host .v-form-item-input,
      .dsm-host .v-form-item-control {
        width: 100%;
        height: 100%;
      }
      .dsm-host .dsm-private-control-shell,
      .dsm-host .dsm-private-input-shell,
      .dsm-host .dsm-private-select-shell,
      .dsm-host .dsm-private-checkbox-shell {
        display: block !important;
        width: 140% !important;
        max-width: none !important;
        min-width: 440px !important;
        margin: 24px !important;
        padding: 18px !important;
        color: #111 !important;
        border: 3px solid #ddd !important;
        background: #fff !important;
        box-shadow: 0 2px 4px #aaa !important;
      }
      .dsm-host .dsm-select,
      .dsm-host [role="combobox"] {
        display: block !important;
        min-width: 440px !important;
        flex-direction: column !important;
        width: 100% !important;
      }
      .dsm-host [role="combobox"] > input,
      .dsm-host [role="combobox"] > button {
        display: block !important;
        min-width: 320px !important;
        width: 100% !important;
        margin: 12px 0 !important;
      }
      .dsm-host .dsm-text-input {
        display: block !important;
        width: 140% !important;
        max-width: none !important;
        min-width: 520px !important;
        margin: 18px !important;
        color: #111 !important;
        background: #fff !important;
      }
      .dsm-host .v-checkbox-wrapper {
        position: relative;
        padding: 2px 0;
      }
      .dsm-host .v-checkbox-icon {
        position: absolute;
        top: 4px;
        left: 0;
        width: 20px;
        height: 20px;
      }
      .dsm-host .v-checkbox-input {
        position: absolute;
        opacity: 0;
        pointer-events: none;
      }
      .dsm-host .v-checkbox-label {
        display: inline-block;
        padding: 2px 0 2px 28px;
        line-height: 20px;
      }
      .dsm-host .v-select2-wrapper {
        position: relative;
        box-sizing: border-box;
        padding: 4px 30px 4px 12px;
        border: 1px solid #8b8b8b;
        background: #efefef;
      }
      .dsm-host .input-wrapper {
        display: flex;
        align-items: center;
        width: 100%;
      }
      .dsm-host .v-select-ul-wrap {
        display: flex;
        flex-wrap: wrap;
        width: 100%;
        margin: 0;
        padding: 0;
      }
      .dsm-host .v-select2-input {
        width: 100%;
        height: 20px;
        padding: 0;
        border: 0;
        background: transparent;
      }
      .dsm-host .select-dropdown {
        position: absolute;
        top: 2px;
        right: 2px;
        width: 24px;
        height: 24px;
      }
      .dsm-owned-decoration {
        width: 12px;
        height: 12px;
        flex: 0 0 12px;
      }
      #variant-rack { width: 340px; margin-top: 20px; }
      .variant { width: 100%; margin-top: 8px; }
    `;
    const html = `<!doctype html>
      <meta charset="utf-8">
      <style id="stale-package-css">${hostileCss}
        .sdsync-app .sdsync-check-row > [class*="checkbox"] [class*="icon"] {
          position: static !important;
          display: inline-grid !important;
          width: 16px !important;
          min-width: 16px !important;
          height: 16px !important;
          margin: 2px 0 0 !important;
          padding: 0 !important;
        }
        .sdsync-app .sdsync-check-row > [class*="checkbox"] > label {
          display: inline-flex !important;
          width: 100% !important;
          min-width: 0;
          min-height: 20px;
          align-items: flex-start;
          gap: 8px;
          margin: 0 !important;
          padding: 0 !important;
        }
        .sdsync-app [class*="select"] [class*="icon"]:not(.sdsync-action-icon) {
          position: static !important;
          display: inline-grid !important;
          width: 22px !important;
          min-width: 22px !important;
          height: 30px !important;
          margin: 0 4px 0 2px !important;
          padding: 0 !important;
          border: 0 !important;
        }
      </style>
      <script>
        const currentStyle = document.createElement("style");
        currentStyle.id = "sdsync-current-runtime-style";
        currentStyle.textContent = ${JSON.stringify(baselineCss)};
        document.head.appendChild(currentStyle);
      </script>
      <body class="dsm-host">
        <div class="sdsync-app" style="display:block !important; width:100% !important; min-height:0 !important; overflow:visible !important">
          <div id="window-shell">
            <form id="settings-panel" class="sdsync-settings-panel">
              ${physicalControlFixture}
              <div id="input-row" class="dsm-form-item sdsync-form-item">
                <div id="input-label-shell" class="dsm-private-label"><label>Search</label></div>
                <div id="input-control-shell" class="dsm-private-control-shell">
                  <div id="input-root" class="sdsync-input-control">
                    <span class="dsm-owned-decoration" aria-hidden="true"></span>
                    <div id="input-shell-one" class="dsm-private-input-shell">
                      <div id="input-shell-two" class="dsm-private-input-shell">
                        <input id="text-input" class="dsm-text-input" value="request identifier">
                      </div>
                    </div>
                  </div>
                </div>
              </div>
            </form>
            <form id="routine-panel" class="sdsync-panel sdsync-horizontal-form sdsync-routine-editor">
              <div class="sdsync-form-grid compact sdsync-routine-fields">
                <div id="routine-row" class="dsm-form-item sdsync-form-item sdsync-inline-form-item">
                  <div id="routine-label-shell" class="v-form-item-label"><label>Mode</label></div>
                  <div id="routine-control-shell" class="v-form-item-input">
                    <div class="v-form-item-control">
                      <div id="routine-input-root" class="sdsync-input-control">
                        <input id="routine-input" class="dsm-text-input" value="realtime">
                      </div>
                    </div>
                  </div>
                </div>
              </div>
            </form>
          </div>
          <div id="variant-rack">
            <div class="variant">
              <div id="select-zero" class="dsm-select sdsync-select-control" role="combobox" aria-haspopup="listbox">
                <input id="select-zero-input" value="zero wrappers">
                <span class="dsm-owned-decoration" aria-hidden="true"></span>
                <button id="select-zero-trigger" type="button" aria-label="Open zero"><svg class="sdsync-action-icon" viewBox="0 0 12 8"><path d="M1 1l5 5 5-5" /></svg></button>
              </div>
            </div>
            <div class="variant">
              <div id="select-one-root" class="dsm-select sdsync-select-control">
                <span class="dsm-owned-decoration" aria-hidden="true"></span>
                <div id="select-one" role="combobox" aria-haspopup="listbox">
                  <input id="select-one-input" value="one wrapper">
                  <button id="select-one-trigger" type="button" aria-label="Open one"><svg class="sdsync-action-icon" viewBox="0 0 12 8"><path d="M1 1l5 5 5-5" /></svg></button>
                </div>
              </div>
            </div>
            <div class="variant">
              <div id="select-two-root" class="dsm-select sdsync-select-control">
                <span class="dsm-owned-decoration" aria-hidden="true"></span>
                <div id="select-two-shell-one" class="dsm-private-select-shell">
                  <div id="select-two-shell-two" class="dsm-private-select-shell">
                    <div id="select-two" role="combobox" aria-haspopup="listbox">
                      <input id="select-two-input" value="two wrappers">
                      <button id="select-two-trigger" type="button" aria-label="Open two"><svg class="sdsync-action-icon" viewBox="0 0 12 8"><path d="M1 1l5 5 5-5" /></svg></button>
                    </div>
                  </div>
                </div>
              </div>
            </div>
          </div>
        </div>
        <div id="shell-app" class="sdsync-app is-dark" style="width:700px;height:220px;margin-top:20px">
          <aside id="shell-sidebar" class="sdsync-sidebar">
            <div class="sdsync-brand"><strong>Drive Sync</strong></div>
            <nav id="shell-nav" class="sdsync-nav">
              ${Array.from({ length: 12 }, (_, index) => `<button class="sdsync-nav-item" type="button">Route ${index + 1}</button>`).join("")}
            </nav>
            <footer id="shell-footer" class="sdsync-sidebar-foot">Package connected</footer>
          </aside>
          <main id="shell-workspace" class="sdsync-workspace">
            <header class="sdsync-topbar"><h1>Settings</h1></header>
            <div id="transition-enter" class="sdsync-page-frame sdsync-page-swap-enter-active sdsync-page-swap-enter"></div>
            <div id="transition-leave" class="sdsync-page-frame sdsync-page-swap-leave-active sdsync-page-swap-leave-to"></div>
          </main>
        </div>
        <script>
          ${inlineControlLayout}
          const requestedWidth = Number(new URLSearchParams(location.search).get("container")) || 900;
          document.getElementById("window-shell").style.width = requestedWidth + "px";
          installControlLayout(document.querySelector(".sdsync-app"));
          const rect = (id) => {
            const value = document.getElementById(id).getBoundingClientRect();
            return { left: value.left, right: value.right, top: value.top, bottom: value.bottom, width: value.width, height: value.height };
          };
          const style = (id) => {
            const value = getComputedStyle(document.getElementById(id));
            return {
              display: value.display,
              flexDirection: value.flexDirection,
              gridTemplateColumns: value.gridTemplateColumns,
              gridColumnStart: value.gridColumnStart,
              gridRowStart: value.gridRowStart,
              gap: value.gap,
              height: value.height,
              marginTop: value.marginTop,
              marginBottom: value.marginBottom,
              marginLeft: value.marginLeft,
              paddingTop: value.paddingTop,
              paddingLeft: value.paddingLeft,
              minWidth: value.minWidth,
              maxWidth: value.maxWidth,
              borderTopWidth: value.borderTopWidth,
              backgroundColor: value.backgroundColor,
              opacity: value.opacity,
              overflowY: value.overflowY,
              position: value.position,
              transitionDuration: value.transitionDuration,
              transitionProperty: value.transitionProperty,
              zIndex: value.zIndex
            };
          };
          const textRect = (id) => {
            const range = document.createRange();
            range.selectNodeContents(document.getElementById(id));
            const value = range.getBoundingClientRect();
            return { left: value.left, right: value.right, top: value.top, bottom: value.bottom, width: value.width, height: value.height };
          };
          const selectVariant = (root, combo, input, trigger, shells = []) => ({
            root: { rect: rect(root), style: style(root) },
            combo: { rect: rect(combo), style: style(combo) },
            input: rect(input),
            trigger: rect(trigger),
            shells: shells.map((id) => rect(id))
          });
          const formRow = (row, label, control) => ({
            row: { rect: rect(row), style: style(row) },
            label: rect(label),
            control: { rect: rect(control), style: style(control) }
          });
          const result = {
            viewport: innerWidth,
            container: rect("settings-panel"),
            selectForm: formRow("select-row", "select-label-shell", "select-control-shell"),
            inputForm: formRow("input-row", "input-label-shell", "input-control-shell"),
            routineForm: formRow("routine-row", "routine-label-shell", "routine-control-shell"),
            formSelect: selectVariant("form-select-root", "form-select-shell-one", "form-select-input", "form-select-trigger", ["form-select-shell-one", "form-select-shell-two"]),
            formSelectPrefixStyle: style("form-select-prefix"),
            formSelectTriggerStyle: style("form-select-trigger"),
            formSelectInputStyle: style("form-select-input"),
            formSelectShellStyles: ["form-select-shell-one", "form-select-shell-two"].map(style),
            selectControlPath: ["select-control-shell", "select-control-inner", "select-control-anonymous"].map((id) => ({ rect: rect(id), style: style(id) })),
            inputRoot: { rect: rect("input-root"), style: style("input-root") },
            inputShells: [rect("input-shell-one"), rect("input-shell-two")],
            inputShellStyles: [style("input-shell-one"), style("input-shell-two")],
            textInputStyle: style("text-input"),
            textInput: rect("text-input"),
            checkRow: rect("check-row"),
            checkboxRoot: { rect: rect("checkbox-root"), style: style("checkbox-root") },
            checkboxLabel: { rect: rect("checkbox-label"), style: style("checkbox-label") },
            checkbox: { rect: rect("checkbox"), style: style("checkbox") },
            checkboxGlyph: { rect: rect("checkbox-decoration"), style: style("checkbox-decoration") },
            checkboxText: textRect("checkbox-label"),
            checkboxHelp: rect("checkbox-help"),
            shell: {
              reducedMotion: matchMedia("(prefers-reduced-motion: reduce)").matches,
              app: rect("shell-app"),
              sidebar: rect("shell-sidebar"),
              nav: Object.assign({ clientHeight: document.getElementById("shell-nav").clientHeight, scrollHeight: document.getElementById("shell-nav").scrollHeight }, { rect: rect("shell-nav"), style: style("shell-nav") }),
              footer: { rect: rect("shell-footer"), style: style("shell-footer") },
              workspace: rect("shell-workspace"),
              enter: style("transition-enter"),
              leave: style("transition-leave")
            },
            variants: [
              selectVariant("select-zero", "select-zero", "select-zero-input", "select-zero-trigger"),
              selectVariant("select-one-root", "select-one", "select-one-input", "select-one-trigger"),
              selectVariant("select-two-root", "select-two", "select-two-input", "select-two-trigger", ["select-two-shell-one", "select-two-shell-two"])
            ]
          };
          document.body.setAttribute("data-layout", btoa(JSON.stringify(result)));
        </script>
      </body>`;
    const htmlPath = join(temporaryDirectory, "layout.html");
    await writeFile(htmlPath, html, "utf8");
    const url = pathToFileURL(htmlPath).href;
    const wide = render(chrome, `${url}?container=900`, join(temporaryDirectory, "profile-wide"));
    const medium = render(chrome, `${url}?container=640`, join(temporaryDirectory, "profile-medium"));
    const narrow = render(chrome, `${url}?container=380`, join(temporaryDirectory, "profile-narrow"));

    for (const form of [wide.selectForm, wide.inputForm, wide.routineForm]) {
      assert.equal(form.row.style.display, "grid");
      assert.match(form.row.style.gridTemplateColumns, /\S+\s+\S+/);
      assert.equal(form.row.style.marginTop, "0px");
      assert.equal(form.row.style.paddingTop, "8px");
      assert.equal(form.control.style.gridColumnStart, "2");
      assert.equal(form.control.style.gridRowStart, "1");
      assert.ok(
        form.control.rect.left >= form.label.right - 1,
        `wide label shell leaked into its control column: ${JSON.stringify(form)}`
      );
      assert.ok(overlapsVertically(form.label, form.control.rect), "wide label and control stacked vertically");
    }

    assert.equal(wide.formSelect.root.style.display, "block", "physical DSM select root owns the surface");
    assert.ok(parseFloat(wide.formSelect.root.style.borderTopWidth) > 0, "select root lost its visible boundary");
    assert.ok(parseFloat(wide.formSelect.root.style.paddingLeft) >= 11, "select root lost its SDK-aligned inset");
    assert.equal(wide.formSelectInputStyle.borderTopWidth, "0px", "inner DSM select input drew a second outline");
    assert.equal(wide.formSelectInputStyle.backgroundColor, "rgba(0, 0, 0, 0)", "inner DSM select input drew a second surface");
    assert.equal(wide.formSelect.root.style.backgroundColor, "rgb(16, 7, 6)", "DSM select root kept a white host surface");
    for (const shell of wide.formSelectShellStyles) {
      assert.equal(shell.backgroundColor, "rgba(0, 0, 0, 0)", "DSM select shell kept a white host surface");
      assert.equal(shell.borderTopWidth, "0px", "DSM select shell kept a duplicate host border");
    }
    assert.equal(wide.formSelectPrefixStyle.display, "none", "empty DSM select prefix icon consumed a column");
    assert.equal(wide.formSelectTriggerStyle.position, "absolute", "dropdown affordance stacked into the select row");
    assert.equal(wide.formSelect.trigger.width, 24);
    assert.equal(wide.formSelect.trigger.height, 24);
    for (const select of [wide.formSelect, ...wide.variants]) {
      assert.match(select.combo.style.display, /^(?:inline-)?flex$/);
      assert.equal(select.combo.style.flexDirection, "row");
      assert.ok(overlapsVertically(select.input, select.trigger), "select input and trigger stacked vertically");
      assert.ok(select.input.width > select.trigger.width, "select trigger consumed the text field");
      assert.ok(select.combo.rect.right <= select.root.rect.right + 0.1, "semantic select overflowed its owned root");
      for (const shell of select.shells) {
        assert.ok(shell.right <= select.root.rect.right + 0.1, "nested private select shell overflowed its root");
        assert.ok(shell.width > select.trigger.width, "nested private select shell collapsed onto its trigger");
      }
    }
    assert.equal(wide.variants[0].root.style.display, "inline-flex");
    for (const select of wide.variants.slice(1)) assert.equal(select.root.style.display, "block");
    for (const shell of wide.selectControlPath) {
      assert.ok(shell.rect.height <= wide.formSelect.root.rect.height + 0.1,
        "DSM form-item input wrapper retained height: 100%");
      assert.equal(shell.style.marginTop, "0px", "DSM form-item input wrapper retained outer margin");
      assert.equal(shell.style.marginLeft, "0px", "DSM form-item input wrapper retained horizontal margin");
      assert.equal(shell.style.paddingTop, "0px", "DSM form-item input wrapper retained outer padding");
      assert.equal(shell.style.backgroundColor, "rgba(0, 0, 0, 0)", "DSM form-item wrapper kept a white host surface");
      assert.equal(shell.style.borderTopWidth, "0px", "DSM form-item wrapper kept a duplicate host border");
    }

    assert.equal(wide.inputRoot.style.display, "inline-flex");
    assert.equal(wide.inputRoot.style.backgroundColor, "rgb(16, 7, 6)", "DSM input root kept a white host surface");
    assert.ok(wide.textInput.width > 200, "nested text input collapsed");
    assert.ok(wide.textInput.right <= wide.inputRoot.rect.right + 0.1, "nested text input overflowed its owned root");
    for (const shell of wide.inputShells) {
      assert.ok(shell.right <= wide.inputRoot.rect.right + 0.1, "nested private input shell overflowed its root");
    }
    for (const shell of wide.inputShellStyles) {
      assert.equal(shell.backgroundColor, "rgba(0, 0, 0, 0)", "nested input shell kept a white host surface");
      assert.equal(shell.borderTopWidth, "0px", "nested input shell kept a duplicate host border");
    }
    assert.equal(wide.textInputStyle.backgroundColor, "rgba(0, 0, 0, 0)", "semantic input obscured its dark owned root");

    assert.equal(wide.checkboxRoot.style.display, "block");
    assert.equal(wide.checkboxLabel.style.display, "inline-block");
    assert.ok(parseFloat(wide.checkboxLabel.style.paddingLeft) >= 28, "checkbox label lost the SDK glyph reservation");
    assert.equal(wide.checkbox.style.position, "absolute");
    assert.equal(wide.checkbox.style.opacity, "0");
    assert.equal(wide.checkboxGlyph.style.position, "absolute");
    assert.equal(wide.checkboxGlyph.rect.width, 20);
    assert.equal(wide.checkboxGlyph.rect.height, 20);
    assert.ok(overlapsVertically(wide.checkboxGlyph.rect, wide.checkboxText), "checkbox glyph and label text stacked vertically");
    assert.ok(wide.checkboxGlyph.rect.right <= wide.checkboxText.left + 1, "checkbox glyph overlapped the label text");
    assert.ok(wide.checkboxRoot.rect.width <= wide.checkRow.width + 0.1, "checkbox root overflowed its owned grid track");
    assert.ok(wide.checkboxRoot.rect.right <= wide.checkboxHelp.left + 0.1, "checkbox label overlapped its help control");
    assert.ok(
      wide.checkboxHelp.right <= wide.checkRow.right + 0.1,
      `checkbox help escaped its owned row: ${JSON.stringify({ row: wide.checkRow, root: wide.checkboxRoot, help: wide.checkboxHelp })}`
    );

    assert.ok(wide.shell.sidebar.bottom <= wide.shell.app.bottom + 0.1, "sidebar escaped the short AppWindow");
    assert.ok(wide.shell.footer.rect.bottom <= wide.shell.app.bottom + 0.1, "status footer escaped the short AppWindow");
    assert.ok(wide.shell.nav.scrollHeight > wide.shell.nav.clientHeight, "short sidebar fixture did not require scrolling");
    assert.equal(wide.shell.nav.style.overflowY, "auto", "sidebar navigation is not independently scrollable");
    assert.ok(wide.shell.nav.rect.bottom <= wide.shell.footer.rect.top + 0.1,
      `scrolling navigation overlaid the status footer: ${JSON.stringify(wide.shell)}`);
    assert.notEqual(wide.shell.footer.style.backgroundColor, "rgba(0, 0, 0, 0)", "status footer remained transparent");
    assert.ok(wide.shell.workspace.left >= wide.shell.sidebar.right - 0.1, "sidebar overlaid the workspace");
    if (wide.shell.reducedMotion) {
      assert.equal(wide.shell.enter.opacity, "1", "reduced-motion mode must not fade route content");
      assert.equal(wide.shell.leave.opacity, "1", "reduced-motion mode must not fade route content");
      assert.equal(wide.shell.enter.transitionDuration, "0s");
      assert.equal(wide.shell.leave.transitionDuration, "0s");
    } else {
      assert.equal(wide.shell.enter.opacity, "0");
      assert.equal(wide.shell.leave.opacity, "0");
      assert.match(wide.shell.enter.transitionProperty, /opacity/);
      assert.match(wide.shell.leave.transitionProperty, /opacity/);
      assert.ok(parseFloat(wide.shell.enter.transitionDuration) >= 0.16, "page enter transition is not smooth");
      assert.ok(parseFloat(wide.shell.leave.transitionDuration) >= 0.16, "page leave transition is not smooth");
    }

    assert.ok(wide.viewport > 720 && medium.viewport > 720 && narrow.viewport > 720, "fixture must keep the DSM browser viewport wide");
    assert.ok(wide.container.width > 420, `wide AppWindow unexpectedly narrow: ${wide.container.width}`);
    assert.ok(medium.container.width > 420, `medium AppWindow unexpectedly compact: ${medium.container.width}`);
    for (const form of [medium.selectForm, medium.inputForm, medium.routineForm]) {
      assert.match(form.row.style.gridTemplateColumns, /\S+\s+\S+/,
        "usable medium AppWindow stacked a label and control");
      assert.equal(form.control.style.gridColumnStart, "2");
      assert.equal(form.control.style.gridRowStart, "1");
      assert.ok(overlapsVertically(form.label, form.control.rect), "medium label and control stacked vertically");
    }
    assert.ok(narrow.container.width <= 420, `narrow AppWindow missed its compact threshold: ${narrow.container.width}`);
    for (const form of [narrow.selectForm, narrow.inputForm, narrow.routineForm]) {
      assert.equal(form.row.style.display, "grid");
      assert.doesNotMatch(form.row.style.gridTemplateColumns, /\S+\s+\S+/);
      assert.equal(form.control.style.gridColumnStart, "1");
      assert.equal(form.control.style.gridRowStart, "2");
      assert.ok(form.control.rect.top >= form.label.bottom - 1, "narrow AppWindow control did not stack below its label");
    }
    assert.equal(narrow.formSelect.combo.style.flexDirection, "row", "narrow form stacked the select internals");
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("resizable AppWindow bounds its shell, owned overlays, tooltips, and secret replacement grid", {
  skip: browserPolicy.skipReason || false
}, async () => {
  assert.ok(chrome, `Chrome/Chromium is required on ${process.platform}/${process.arch} CI`);
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "sdsync-shell-boundary-"));
  try {
    const html = `<!doctype html>
      <meta charset="utf-8">
      <style>
        html, body { margin: 0; width: 1100px; height: 1000px; overflow: hidden; }
        ${baselineCss}
        #app-shell { position: relative; height: 700px; margin: 20px 0 0 80px; }
        #secret-panel { position: relative; width: 100%; min-width: 0; }
        #tip-owner { position: absolute; right: 8px; bottom: 8px; }
        #unrelated-overlay { position: fixed; left: 1030px; top: 860px; width: 420px; height: 120px; }
        .fixture-secret-control { min-height: 34px; padding: 7px; border: 1px solid currentColor; }
      </style>
      <body>
        <div id="app-shell" class="sdsync-app is-dark">
          <aside id="responsive-sidebar" class="sdsync-sidebar">
            <div class="sdsync-brand"><span aria-hidden="true">DS</span><div><strong>Drive Sync</strong><span>File Station sync</span></div></div>
            <nav class="sdsync-nav"><button class="sdsync-nav-item"><span class="sdsync-nav-icon">A</span><span>Activity</span></button></nav>
            <footer class="sdsync-sidebar-foot"><span class="sdsync-connection-dot"></span><span>Package connected</span></footer>
          </aside>
          <main id="responsive-workspace" class="sdsync-workspace">
            <section id="secret-panel" class="sdsync-panel">
              <div id="secret-editor" class="sdsync-secret-editor">
                <div id="secret-summary" class="sdsync-secret-summary"><strong>Password</strong><span>Stored · masked</span></div>
                <div id="secret-mode" class="sdsync-secret-mode fixture-secret-control">Replace</div>
                <span id="secret-mode-help" class="sdsync-secret-mode-help">?</span>
                <div id="secret-value" class="sdsync-secret-value fixture-secret-control">replacement</div>
                <span id="secret-value-help" class="sdsync-secret-value-help">?</span>
              </div>
            </section>
            <span id="tip-owner" class="sdsync-field-tip">
              <button class="sdsync-field-tip-trigger" type="button">?</button>
              <span id="field-tip" class="sdsync-field-tip-content" role="tooltip">A bounded field explanation that remains readable beside the right and bottom AppWindow edges.</span>
            </span>
          </main>
        </div>
        <div id="unrelated-overlay">DSM-owned overlay</div>
        <script>
          ${inlineControlLayout}
          const requestedWidth = Number(new URLSearchParams(location.search).get("shell")) || 900;
          const app = document.getElementById("app-shell");
          app.style.width = requestedWidth + "px";
          const cleanup = installControlLayout(app);
          const packageOverlay = document.createElement("div");
          packageOverlay.id = "package-overlay";
          packageOverlay.className = "sdsync-select-dropdown is-dark";
          packageOverlay.textContent = "Package select menu";
          packageOverlay.style.cssText = "position:fixed;left:1040px;top:890px;width:500px;height:240px";
          document.body.appendChild(packageOverlay);
          const rect = (id) => {
            const value = document.getElementById(id).getBoundingClientRect();
            return { left: value.left, right: value.right, top: value.top, bottom: value.bottom, width: value.width, height: value.height };
          };
          const grid = (id) => {
            const value = getComputedStyle(document.getElementById(id));
            return { columnStart: value.gridColumnStart, columnEnd: value.gridColumnEnd, rowStart: value.gridRowStart, rowEnd: value.gridRowEnd };
          };
          setTimeout(() => {
            const workspace = document.getElementById("responsive-workspace");
            const before = {
              shell: rect("app-shell"),
              shellClasses: app.className,
              shellClientWidth: app.clientWidth,
              shellScrollWidth: app.scrollWidth,
              sidebar: rect("responsive-sidebar"),
              workspace: rect("responsive-workspace"),
              workspaceClientWidth: workspace.clientWidth,
              workspaceScrollWidth: workspace.scrollWidth,
              overlay: rect("package-overlay"),
              overlayClasses: packageOverlay.className,
              overlayPosition: packageOverlay.style.position,
              overlayLeft: packageOverlay.style.left,
              overlayTop: packageOverlay.style.top,
              overlayStyleAttribute: packageOverlay.getAttribute("style"),
              overlayComputedPosition: getComputedStyle(packageOverlay).position,
              overlayComputedLeft: getComputedStyle(packageOverlay).left,
              overlayComputedTop: getComputedStyle(packageOverlay).top,
              overlayTransform: getComputedStyle(packageOverlay).transform,
              overlayMaxWidth: packageOverlay.style.getPropertyValue("max-width"),
              tooltip: rect("field-tip"),
              tooltipOwner: rect("tip-owner"),
              tooltipClasses: document.getElementById("field-tip").className,
              secret: {
                summary: { rect: rect("secret-summary"), grid: grid("secret-summary") },
                mode: { rect: rect("secret-mode"), grid: grid("secret-mode") },
                modeHelp: { rect: rect("secret-mode-help"), grid: grid("secret-mode-help") },
                value: { rect: rect("secret-value"), grid: grid("secret-value") },
                valueHelp: { rect: rect("secret-value-help"), grid: grid("secret-value-help") }
              },
              unrelatedStyle: document.getElementById("unrelated-overlay").getAttribute("style") || ""
            };
            cleanup();
            const after = {
              shellClasses: app.className,
              overlayClasses: packageOverlay.className,
              overlayPosition: packageOverlay.style.position,
              overlayLeft: packageOverlay.style.left,
              overlayTop: packageOverlay.style.top,
              overlayMaxWidth: packageOverlay.style.maxWidth,
              tooltipClasses: document.getElementById("field-tip").className,
              tooltipLeft: document.getElementById("field-tip").style.getPropertyValue("--sdsync-tip-left"),
              unrelatedStyle: document.getElementById("unrelated-overlay").getAttribute("style") || ""
            };
            document.body.setAttribute("data-layout", btoa(JSON.stringify({ before, after })));
          }, 80);
        </script>
      </body>`;
    const htmlPath = join(temporaryDirectory, "shell-boundary.html");
    await writeFile(htmlPath, html, "utf8");
    const url = pathToFileURL(htmlPath).href;
    const boundaryRender = { retry: false, timeoutMs: 15000 };
    const wide = render(chrome, `${url}?shell=900`, join(temporaryDirectory, "profile-shell-wide"), boundaryRender);
    const medium = render(chrome, `${url}?shell=640`, join(temporaryDirectory, "profile-shell-medium"), boundaryRender);
    const narrow = render(chrome, `${url}?shell=380`, join(temporaryDirectory, "profile-shell-narrow"), boundaryRender);

    for (const result of [wide, medium, narrow]) {
      const { before, after } = result;
      assert.ok(before.shellScrollWidth <= before.shellClientWidth,
        `AppWindow gained horizontal overflow: ${JSON.stringify(before)}`);
      assert.ok(before.workspaceScrollWidth <= before.workspaceClientWidth,
        `workspace gained horizontal overflow: ${JSON.stringify(before)}`);
      assert.ok(before.sidebar.right <= before.workspace.left + 0.1, "sidebar overlaid the workspace");
      assert.ok(before.workspace.right <= before.shell.right + 0.1, "workspace escaped the AppWindow");
      assert.match(before.overlayClasses, /\bsdsync-overlay-bounded\b/);
      assert.ok(before.overlay.left >= before.shell.left + 7.9, "package dropdown escaped the left edge");
      assert.ok(before.overlay.right <= before.shell.right - 7.9,
        `package dropdown escaped the right edge: ${JSON.stringify({ shell: before.shell, overlay: before.overlay, position: before.overlayPosition, left: before.overlayLeft, style: before.overlayStyleAttribute, computedPosition: before.overlayComputedPosition, computedLeft: before.overlayComputedLeft, transform: before.overlayTransform })}`);
      assert.ok(before.overlay.top >= before.shell.top + 7.9,
        `package dropdown escaped the top edge: ${JSON.stringify({ shell: before.shell, overlay: before.overlay, top: before.overlayTop })}`);
      assert.ok(before.overlay.bottom <= before.shell.bottom - 7.9,
        `package dropdown escaped the bottom edge: ${JSON.stringify({ shell: before.shell, overlay: before.overlay, top: before.overlayTop })}`);
      assert.match(before.tooltipClasses, /\bsdsync-tip-bounded\b/);
      assert.ok(before.tooltip.left >= before.shell.left + 7.9, "field tooltip escaped the left edge");
      assert.ok(before.tooltip.right <= before.shell.right - 7.9, "field tooltip escaped the right edge");
      assert.ok(before.tooltip.top >= before.shell.top + 7.9, "field tooltip escaped the top edge");
      assert.ok(before.tooltip.bottom <= before.shell.bottom - 7.9, "field tooltip escaped the bottom edge");
      assert.ok(before.tooltip.bottom <= before.tooltipOwner.top,
        "bottom-edge tooltip was not placed above its trigger");
      assert.doesNotMatch(after.shellClasses, /sdsync-(?:medium|compact)-shell/);
      assert.doesNotMatch(after.overlayClasses, /sdsync-overlay-bounded/);
      assert.equal(after.overlayPosition, "fixed");
      assert.equal(after.overlayLeft, "1040px");
      assert.equal(after.overlayTop, "890px");
      assert.equal(after.overlayMaxWidth, "");
      assert.doesNotMatch(after.tooltipClasses, /sdsync-tip-bounded/);
      assert.equal(after.tooltipLeft, "");
      assert.equal(before.unrelatedStyle, after.unrelatedStyle, "unrelated DSM overlay was mutated");
    }

    assert.match(wide.before.shellClasses, /\bsdsync-medium-shell\b/);
    assert.doesNotMatch(wide.before.shellClasses, /\bsdsync-compact-shell\b/);
    for (const result of [medium, narrow]) {
      assert.match(result.before.shellClasses, /\bsdsync-medium-shell\b/);
      assert.match(result.before.shellClasses, /\bsdsync-compact-shell\b/);
    }
    assert.equal(wide.before.secret.summary.grid.columnStart, "1");
    assert.equal(wide.before.secret.summary.grid.rowStart, "1");
    assert.equal(wide.before.secret.mode.grid.columnStart, "2");
    assert.equal(wide.before.secret.mode.grid.rowStart, "1");
    assert.equal(wide.before.secret.modeHelp.grid.columnStart, "3");
    assert.equal(wide.before.secret.value.grid.columnStart, "2");
    assert.equal(wide.before.secret.value.grid.rowStart, "2");
    assert.equal(wide.before.secret.valueHelp.grid.columnStart, "3");
    for (const result of [medium, narrow]) {
      assert.equal(result.before.secret.summary.grid.columnStart, "1");
      assert.equal(result.before.secret.summary.grid.rowStart, "1");
      assert.equal(result.before.secret.mode.grid.columnStart, "1");
      assert.equal(result.before.secret.mode.grid.rowStart, "2");
      assert.equal(result.before.secret.modeHelp.grid.columnStart, "2");
      assert.equal(result.before.secret.value.grid.columnStart, "1");
      assert.equal(result.before.secret.value.grid.rowStart, "3");
      assert.equal(result.before.secret.valueHelp.grid.columnStart, "2");
      assert.ok(overlapsVertically(result.before.secret.mode.rect, result.before.secret.modeHelp.rect));
      assert.ok(overlapsVertically(result.before.secret.value.rect, result.before.secret.valueHelp.rect));
    }
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});
