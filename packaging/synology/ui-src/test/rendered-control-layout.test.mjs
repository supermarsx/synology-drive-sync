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
const baselineCss = css.slice(0, css.indexOf("@container (max-width: 520px)"));
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

function cssGridTrackCount(value) {
  const source = String(value || "").trim();
  if (!source || source === "none") return 0;
  let depth = 0;
  let tracks = 1;
  let betweenTracks = false;
  for (const character of source) {
    if (character === "(") depth += 1;
    else if (character === ")") depth = Math.max(0, depth - 1);
    else if (/\s/.test(character) && depth === 0) betweenTracks = true;
    else if (betweenTracks && depth === 0) {
      tracks += 1;
      betweenTracks = false;
    }
  }
  return tracks;
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
  assert.equal(cssGridTrackCount("minmax(0px, 1fr)"), 1);
  assert.equal(cssGridTrackCount("190px minmax(0px, 1fr)"), 2);
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
      .dsm-host .v-form-item-input.fit-container,
      .dsm-host .v-form-item-control.fit-container,
      .dsm-host .v-textfield.fit-container,
      .dsm-host .v-textfield-input.fit-container,
      .dsm-host .v-textfield-input-inner.fit-container {
        display: inline-block !important;
        inline-size: 84px !important;
        width: 84px !important;
        max-inline-size: 84px !important;
        max-width: 84px !important;
        min-inline-size: 84px !important;
        min-width: 84px !important;
        flex: 0 0 84px !important;
        margin: 19px !important;
        padding: 13px !important;
        background: #fff !important;
      }
      .dsm-host input.v-textfield-input-element.fit-container,
      .dsm-host textarea.v-textfield-input-element.fit-container {
        inline-size: 64px !important;
        width: 64px !important;
        max-inline-size: 64px !important;
        max-width: 64px !important;
        min-inline-size: 64px !important;
        min-width: 64px !important;
        flex: 0 0 64px !important;
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
            <div id="profile-layout" class="sdsync-profiles-layout is-editor-only">
              <form id="profile-editor" class="sdsync-panel sdsync-editor sdsync-profile-editor">
                <div id="profile-main-grid" class="sdsync-form-grid">
                  <div id="editor-row" class="dsm-form-item sdsync-form-item">
                    <div id="editor-label-shell" class="v-form-item-label"><label>Target</label></div>
                    <div id="editor-control-shell" class="v-form-item-input">
                      <div class="v-form-item-control">
                        <div id="editor-input-root" class="sdsync-input-control">
                          <input id="editor-input" class="dsm-text-input" value="https://nas.example.test">
                        </div>
                      </div>
                    </div>
                  </div>
                  <div id="editor-source-row" class="dsm-form-item sdsync-form-item">
                    <div id="editor-source-label" class="v-form-item-label"><label>Local source</label></div>
                    <div id="editor-source-control" class="v-form-item-input">
                      <div class="v-form-item-control"><div class="sdsync-input-control"><input class="dsm-text-input" value="/volume1/source"></div></div>
                    </div>
                  </div>
                </div>
                <details id="profile-advanced" class="sdsync-advanced" open>
                  <summary><strong>Advanced profile controls</strong><span>Network and retry policy</span></summary>
                  <div id="profile-advanced-grid" class="sdsync-form-grid">
                    <div id="advanced-row" class="dsm-form-item sdsync-form-item">
                      <div id="advanced-label" class="v-form-item-label"><label>Retries</label></div>
                      <div id="advanced-control" class="v-form-item-input">
                        <div class="v-form-item-control"><div class="sdsync-input-control"><input class="dsm-text-input" value="3"></div></div>
                      </div>
                    </div>
                    <div id="advanced-toggle-row" class="sdsync-toggle-row">
                      <span id="advanced-toggle-label" class="sdsync-toggle-label">Quiet terminal sink</span>
                      <div id="advanced-checkbox-root" class="sdsync-checkbox-control"><input type="checkbox" checked aria-label="Quiet terminal sink"></div>
                    </div>
                  </div>
                </details>
                <fieldset id="profile-danger" class="sdsync-danger-fieldset">
                  <legend>Deletion guard</legend>
                  <div id="danger-toggle-row" class="sdsync-toggle-row is-danger">
                    <span id="danger-toggle-label" class="sdsync-toggle-label">Mirror remote deletions</span>
                    <div id="danger-checkbox-root" class="sdsync-checkbox-control"><input type="checkbox" aria-label="Mirror remote deletions"></div>
                  </div>
                  <div id="danger-row" class="dsm-form-item sdsync-form-item">
                    <div id="danger-label" class="v-form-item-label"><label>Maximum deletions per run</label></div>
                    <div id="danger-control" class="v-form-item-input">
                      <div class="v-form-item-control"><div class="sdsync-input-control"><input class="dsm-text-input" value="100"></div></div>
                    </div>
                  </div>
                </fieldset>
              </form>
            </div>
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
           const overflow = (id) => {
             const value = document.getElementById(id);
             return { clientWidth: value.clientWidth, scrollWidth: value.scrollWidth };
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
               borderTopColor: value.borderTopColor,
               backgroundColor: value.backgroundColor,
               backgroundImage: value.backgroundImage,
               opacity: value.opacity,
               pointerEvents: value.pointerEvents,
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
             windowShellOverflow: overflow("window-shell"),
             selectForm: formRow("select-row", "select-label-shell", "select-control-shell"),
             inputForm: formRow("input-row", "input-label-shell", "input-control-shell"),
             textareaForm: formRow("textarea-row", "textarea-label-shell", "textarea-control-shell"),
             routineForm: formRow("routine-row", "routine-label-shell", "routine-control-shell"),
             editorForm: formRow("editor-row", "editor-label-shell", "editor-control-shell"),
             editor: {
               rect: rect("profile-editor"),
               compact: document.getElementById("profile-editor").classList.contains("sdsync-compact-form")
             },
             profile: {
               layout: { rect: rect("profile-layout"), style: style("profile-layout"), overflow: overflow("profile-layout") },
               editor: { rect: rect("profile-editor"), style: style("profile-editor") },
               mainGrid: { rect: rect("profile-main-grid"), style: style("profile-main-grid") },
               advancedGrid: { rect: rect("profile-advanced-grid"), style: style("profile-advanced-grid") },
               danger: { rect: rect("profile-danger"), style: style("profile-danger") },
               rows: {
                 target: formRow("editor-row", "editor-label-shell", "editor-control-shell"),
                 source: formRow("editor-source-row", "editor-source-label", "editor-source-control"),
                 advanced: formRow("advanced-row", "advanced-label", "advanced-control"),
                 danger: formRow("danger-row", "danger-label", "danger-control")
               },
               toggles: {
                 advanced: { row: rect("advanced-toggle-row"), label: rect("advanced-toggle-label"), control: rect("advanced-checkbox-root") },
                 danger: { row: rect("danger-toggle-row"), label: rect("danger-toggle-label"), control: rect("danger-checkbox-root") }
               }
             },
            formSelect: selectVariant("form-select-root", "form-select-shell-one", "form-select-input", "form-select-trigger", ["form-select-shell-one", "form-select-shell-two"]),
            formSelectPrefixStyle: style("form-select-prefix"),
            formSelectTriggerStyle: style("form-select-trigger"),
            formSelectInputStyle: style("form-select-input"),
            formSelectShellStyles: ["form-select-shell-one", "form-select-shell-two"].map(style),
            selectControlPath: ["select-control-shell", "select-control-inner", "select-control-anonymous"].map((id) => ({ rect: rect(id), style: style(id) })),
             inputRoot: { rect: rect("input-root"), style: style("input-root") },
             inputControlPath: ["input-control-shell", "input-control-inner", "input-control-anonymous"].map((id) => ({ rect: rect(id), style: style(id) })),
             inputShells: [rect("input-shell-one"), rect("input-shell-two")],
             inputShellStyles: [style("input-shell-one"), style("input-shell-two")],
             textInputStyle: style("text-input"),
             textInput: rect("text-input"),
             textareaRoot: { rect: rect("textarea-root"), style: style("textarea-root") },
             textareaControlPath: ["textarea-control-shell", "textarea-control-inner", "textarea-control-anonymous"].map((id) => ({ rect: rect(id), style: style(id) })),
             textareaShell: { rect: rect("textarea-shell-one"), style: style("textarea-shell-one") },
             textareaInput: { rect: rect("textarea-input"), style: style("textarea-input") },
             checkRow: rect("check-row"),
             toggleLabel: { rect: rect("toggle-label"), style: style("toggle-label") },
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
    const profileRows = (layout) => Object.values(layout.profile.rows);

    for (const form of [wide.selectForm, wide.inputForm, wide.textareaForm, wide.routineForm, ...profileRows(wide)]) {
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
      assert.ok(form.control.rect.right >= form.row.rect.right - 1,
        `wide control did not fill its horizontal form track: ${JSON.stringify(form)}`);
      assert.ok(form.control.rect.width >= form.row.rect.width * 0.5,
        `wide control was crunched by its label track: ${JSON.stringify(form)}`);
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

    for (const [layoutName, layout] of [["wide", wide], ["medium", medium], ["narrow", narrow]]) {
      for (const control of [
        {
          name: "input",
          form: layout.inputForm,
          root: layout.inputRoot,
          semantic: { rect: layout.textInput, style: layout.textInputStyle },
          shells: layout.inputShells.map((rect, index) => ({ rect, style: layout.inputShellStyles[index] })),
          path: layout.inputControlPath
        },
        {
          name: "textarea",
          form: layout.textareaForm,
          root: layout.textareaRoot,
          semantic: layout.textareaInput,
          shells: [layout.textareaShell],
          path: layout.textareaControlPath
        }
      ]) {
        const context = `${layoutName} ${control.name}`;
        assert.equal(control.root.style.backgroundColor, "rgb(16, 7, 6)", `${context} root kept a white DSM surface`);
        assert.ok(control.root.rect.width >= control.form.control.rect.width - 1,
          `${context} root did not fill its form control track`);
        assert.ok(control.semantic.rect.width >= control.root.rect.width * 0.75,
          `${context} semantic control remained crunched by fit-container`);
        assert.ok(control.semantic.rect.right <= control.root.rect.right + 0.1,
          `${context} semantic control overflowed its owned root`);
        assert.equal(control.semantic.style.minWidth, "0px",
          `${context} semantic control retained the fit-container minimum width`);
        assert.notEqual(control.semantic.style.maxWidth, "64px",
          `${context} semantic control retained the fit-container maximum width`);
        assert.equal(control.semantic.style.marginLeft, "0px",
          `${context} semantic control retained a hostile outer margin`);
        assert.equal(control.semantic.style.backgroundColor, "rgba(0, 0, 0, 0)",
          `${context} semantic control obscured its dark owned root`);
        for (const shell of control.shells) {
          assert.ok(shell.rect.width >= control.root.rect.width * 0.75,
            `${context} nested textfield shell remained fit-content instead of using the owned root`);
          assert.ok(shell.rect.right <= control.root.rect.right + 0.1,
            `${context} nested textfield shell overflowed its owned root`);
          assert.equal(shell.style.marginLeft, "0px", `${context} DSM shell retained a hostile outer margin`);
          assert.equal(shell.style.backgroundColor, "rgba(0, 0, 0, 0)", `${context} DSM shell kept a white host surface`);
        }
        for (const shell of control.path) {
          assert.ok(shell.rect.width >= control.form.control.rect.width - 1,
            `${context} form-item shell remained fit-content instead of filling the control track`);
          assert.ok(shell.rect.right <= control.form.control.rect.right + 0.1,
            `${context} form-item shell overflowed its control track`);
          assert.equal(shell.style.marginLeft, "0px", `${context} form-item shell retained a hostile outer margin`);
          assert.equal(shell.style.backgroundColor, "rgba(0, 0, 0, 0)", `${context} form-item shell kept a white host surface`);
        }
      }
      assert.ok(layout.windowShellOverflow.scrollWidth <= layout.windowShellOverflow.clientWidth + 1,
        `${layoutName} AppWindow acquired horizontal overflow from a form control`);

      for (const grid of [layout.profile.layout, layout.profile.mainGrid, layout.profile.advancedGrid, layout.profile.danger]) {
        assert.equal(cssGridTrackCount(grid.style.gridTemplateColumns), 1,
          `${layoutName} profile editor did not keep exactly one configuration track: ${grid.style.gridTemplateColumns}`);
      }
      assert.ok(layout.profile.editor.rect.width >= layout.profile.layout.rect.width - 1,
        `${layoutName} dedicated profile editor did not fill its catalog-replacement track`);
      assert.ok(layout.profile.layout.overflow.scrollWidth <= layout.profile.layout.overflow.clientWidth + 1,
        `${layoutName} dedicated profile editor introduced horizontal overflow`);
      assert.ok(layout.profile.rows.source.row.rect.top >= layout.profile.rows.target.row.rect.bottom - 1,
        `${layoutName} main profile fields shared a visual line`);
      assert.ok(layout.profile.toggles.advanced.row.top >= layout.profile.rows.advanced.row.rect.bottom - 1,
        `${layoutName} advanced field and toggle shared a visual line`);
      assert.ok(layout.profile.rows.danger.row.rect.top >= layout.profile.toggles.danger.row.bottom - 1,
        `${layoutName} danger toggle and field shared a visual line`);
      for (const toggle of Object.values(layout.profile.toggles)) {
        assert.ok(toggle.label.right <= toggle.control.left + 1,
          `${layoutName} profile toggle overlapped its label`);
        assert.ok(toggle.control.right >= toggle.row.right - 1,
          `${layoutName} profile toggle did not remain at the right edge of its own row`);
      }
    }

    for (const layout of [wide, medium, narrow]) {
      assert.match(layout.checkboxRoot.style.display, /^(?:inline-)?grid$/);
      assert.equal(layout.checkboxRoot.rect.width, 22);
      assert.equal(layout.checkboxRoot.rect.height, 22);
      assert.equal(layout.checkboxLabel.style.display, "none", "DSM checkbox label remained visibly duplicated");
      assert.equal(layout.checkboxGlyph.style.display, "none", "DSM checkbox glyph remained visibly vendor-themed");
      assert.equal(layout.checkbox.style.position, "relative");
      assert.equal(layout.checkbox.style.opacity, "1");
      assert.equal(layout.checkbox.style.pointerEvents, "auto");
      assert.equal(layout.checkbox.rect.width, 22);
      assert.equal(layout.checkbox.rect.height, 22);
      assert.equal(layout.checkbox.style.backgroundColor, "rgb(255, 106, 26)",
        "checked semantic toggle lost the hellfire surface");
      assert.notEqual(layout.checkbox.style.backgroundImage, "none",
        "checked semantic toggle lost its package-owned mark");
      assert.ok(layout.toggleLabel.rect.right <= layout.checkboxRoot.rect.left + 1,
        "right-hand toggle overlapped its visible label");
      assert.ok(layout.checkboxRoot.rect.right >= layout.checkRow.right - 1,
        "semantic toggle did not stay at the row's right edge");
      assert.ok(layout.checkboxHelp.right <= layout.toggleLabel.rect.right + 0.1,
        "toggle help escaped its visible label boundary");
      assert.ok(layout.checkboxRoot.rect.right <= layout.checkRow.right + 0.1,
        "semantic toggle escaped its owned row");
    }

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
    assert.ok(wide.container.width > 520, `wide AppWindow unexpectedly narrow: ${wide.container.width}`);
    assert.ok(medium.container.width > 520, `medium AppWindow unexpectedly compact: ${medium.container.width}`);
    assert.equal(medium.editor.compact, false, "editor observer marked a 640px editor compact");
    for (const form of [medium.selectForm, medium.inputForm, medium.textareaForm, medium.routineForm, ...profileRows(medium)]) {
      assert.match(form.row.style.gridTemplateColumns, /\S+\s+\S+/,
        "usable medium AppWindow stacked a label and control");
      assert.equal(form.control.style.gridColumnStart, "2");
      assert.equal(form.control.style.gridRowStart, "1");
      assert.ok(overlapsVertically(form.label, form.control.rect), "medium label and control stacked vertically");
      assert.ok(form.control.rect.right >= form.row.rect.right - 1,
        "medium control did not fill its horizontal form track");
      assert.ok(form.control.rect.width >= form.row.rect.width * 0.55,
        "medium control was crunched by its label track");
    }
    assert.ok(narrow.container.width <= 520, `narrow AppWindow missed its compact threshold: ${narrow.container.width}`);
    assert.equal(narrow.editor.compact, true, "editor observer missed a sub-520px editor");
    for (const [name, form] of Object.entries({
      select: narrow.selectForm,
      input: narrow.inputForm,
      textarea: narrow.textareaForm,
      routine: narrow.routineForm,
      profileTarget: narrow.profile.rows.target,
      profileSource: narrow.profile.rows.source,
      profileAdvanced: narrow.profile.rows.advanced,
      profileDanger: narrow.profile.rows.danger
    })) {
      assert.equal(form.row.style.display, "grid");
      assert.doesNotMatch(form.row.style.gridTemplateColumns, /\S+\s+\S+/,
        `${name} row did not stack at the 520px compact threshold`);
      assert.equal(form.control.style.gridColumnStart, "1");
      assert.equal(form.control.style.gridRowStart, "2");
      assert.ok(form.control.rect.top >= form.label.bottom - 1, "narrow AppWindow control did not stack below its label");
      assert.ok(form.control.rect.width >= form.row.rect.width - 1,
        "compact form control did not expand to the full row width");
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

test("file-explorer folder picker stays contained and scrollable across DSM AppWindow sizes", {
  skip: browserPolicy.skipReason || false
}, async () => {
  assert.ok(chrome, `Chrome/Chromium is required on ${process.platform}/${process.arch} CI`);
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "sdsync-folder-explorer-"));
  try {
    const folderRows = Array.from({ length: 18 }, (_, index) => `
      <div class="sdsync-path-browser-row" role="listitem">
        <button class="sdsync-path-browser-open" type="button" aria-label="Open folder Archive ${index + 1}">
          <span class="sdsync-path-browser-folder-icon" aria-hidden="true">F</span>
          <span class="sdsync-path-browser-folder-copy">
            <strong class="sdsync-path-browser-folder-name">Archive ${index + 1}</strong>
            <code class="sdsync-path-browser-folder-path">/volume1/Shared/Department/Long project name/Archive ${index + 1}</code>
          </span>
          <span aria-hidden="true">&gt;</span>
        </button>
        <button class="fixture-button" type="button" aria-label="Select folder Archive ${index + 1}">Select</button>
      </div>`).join("");
    const html = `<!doctype html>
      <meta charset="utf-8">
      <style>
        html, body { margin: 0; width: 1100px; height: 1000px; overflow: hidden; }
        ${baselineCss}
        #folder-app { position: relative; display: block !important; margin: 12px; }
        .fixture-button { min-width: 0; min-height: 30px; padding: 5px 8px; border: 1px solid var(--sdsync-control-border); color: var(--sdsync-text); background: var(--sdsync-control); }
      </style>
      <body>
        <div id="folder-app" class="sdsync-app is-dark">
          <div id="folder-backdrop" class="sdsync-modal-backdrop sdsync-path-browser-backdrop">
            <div id="folder-dialog" class="sdsync-modal sdsync-path-browser" role="dialog" aria-modal="true" aria-labelledby="folder-title" aria-describedby="folder-description" tabindex="-1">
              <header id="folder-header" class="sdsync-path-browser-header">
                <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Folder explorer</p><h2 id="folder-title">Choose a local NAS source</h2></div><button class="fixture-button" type="button">Close</button></div>
              </header>
              <p id="folder-description" class="sdsync-path-browser-intro">Only canonical NAS directories readable and traversable by the package identity are shown.</p>
              <div id="folder-toolbar" class="sdsync-path-browser-toolbar">
                <button class="fixture-button" type="button">Parent folder</button>
                <nav id="folder-breadcrumbs" class="sdsync-path-browser-breadcrumbs" aria-label="Current folder"><ol>
                  <li><button class="sdsync-path-browser-crumb" type="button">NAS</button><span class="sdsync-path-browser-separator" aria-hidden="true">/</span></li>
                  <li><button class="sdsync-path-browser-crumb" type="button">volume1</button><span class="sdsync-path-browser-separator" aria-hidden="true">/</span></li>
                  <li><button class="sdsync-path-browser-crumb" type="button">Shared</button><span class="sdsync-path-browser-separator" aria-hidden="true">/</span></li>
                  <li><button class="sdsync-path-browser-crumb" type="button">Department</button><span class="sdsync-path-browser-separator" aria-hidden="true">/</span></li>
                  <li><button class="sdsync-path-browser-crumb" type="button" aria-current="location" disabled>Long project name</button></li>
                </ol></nav>
              </div>
              <section id="folder-main" class="sdsync-path-browser-main" aria-label="Folder contents">
                <div id="folder-current" class="sdsync-path-browser-current" aria-live="polite"><span aria-hidden="true">F</span><span class="sdsync-path-browser-current-copy"><span class="sdsync-path-browser-current-label">Current folder</span><code>/volume1/Shared/Department/Long project name</code></span></div>
                <div class="sdsync-path-browser-columns" aria-hidden="true"><span>Name</span><span>Choose</span></div>
                <div id="folder-list" class="sdsync-path-browser-list" role="list" aria-busy="false">${folderRows}</div>
              </section>
              <footer id="folder-footer" class="sdsync-path-browser-footer">
                <span class="sdsync-path-browser-summary"><span>18 folders visible</span><code>/volume1/Shared/Department/Long project name</code></span>
                <span class="sdsync-path-browser-footer-actions"><button class="fixture-button" type="button">Cancel</button><button class="fixture-button" type="button">Select this folder</button></span>
              </footer>
            </div>
          </div>
        </div>
        <script>
          ${inlineControlLayout}
          const parameters = new URLSearchParams(location.search);
          const app = document.getElementById("folder-app");
          app.style.width = (Number(parameters.get("width")) || 760) + "px";
          app.style.height = (Number(parameters.get("height")) || 480) + "px";
          installControlLayout(app);
          const rect = (id) => {
            const value = document.getElementById(id).getBoundingClientRect();
            return { left: value.left, right: value.right, top: value.top, bottom: value.bottom, width: value.width, height: value.height };
          };
          const overflow = (id) => {
            const value = document.getElementById(id);
            const style = getComputedStyle(value);
            return { clientWidth: value.clientWidth, scrollWidth: value.scrollWidth, clientHeight: value.clientHeight, scrollHeight: value.scrollHeight, overflowX: style.overflowX, overflowY: style.overflowY };
          };
          setTimeout(() => {
            document.body.setAttribute("data-layout", btoa(JSON.stringify({
              compact: app.classList.contains("sdsync-compact-shell"),
              short: app.classList.contains("sdsync-short-shell"),
              app: { rect: rect("folder-app"), overflow: overflow("folder-app") },
              backdrop: rect("folder-backdrop"),
              dialog: { rect: rect("folder-dialog"), overflow: overflow("folder-dialog"), gridTemplateRows: getComputedStyle(document.getElementById("folder-dialog")).gridTemplateRows },
              header: rect("folder-header"),
              toolbar: { rect: rect("folder-toolbar"), overflow: overflow("folder-toolbar") },
              breadcrumbs: { rect: rect("folder-breadcrumbs"), overflow: overflow("folder-breadcrumbs") },
              main: rect("folder-main"),
              current: rect("folder-current"),
              list: { rect: rect("folder-list"), overflow: overflow("folder-list") },
              openDisplay: getComputedStyle(document.querySelector(".sdsync-path-browser-open")).display,
              footer: rect("folder-footer")
            })));
          }, 80);
        </script>
      </body>`;
    const htmlPath = join(temporaryDirectory, "folder-explorer.html");
    await writeFile(htmlPath, html, "utf8");
    const url = pathToFileURL(htmlPath).href;
    const browserRender = { retry: false, timeoutMs: 15000 };
    const tall = render(chrome, `${url}?width=760&height=640`, join(temporaryDirectory, "folder-tall"), browserRender);
    const wide = render(chrome, `${url}?width=760&height=480`, join(temporaryDirectory, "folder-wide"), browserRender);
    const compact = render(chrome, `${url}?width=500&height=360`, join(temporaryDirectory, "folder-compact"), browserRender);
    const narrow = render(chrome, `${url}?width=390&height=360`, join(temporaryDirectory, "folder-narrow"), browserRender);

    assert.equal(tall.compact, false);
    assert.equal(tall.short, false);
    assert.equal(wide.compact, false);
    assert.equal(wide.short, true);
    assert.equal(compact.compact, true);
    assert.equal(compact.short, true);
    assert.equal(narrow.compact, true);
    assert.equal(narrow.short, true);
    for (const result of [tall, wide, compact, narrow]) {
      assert.ok(result.app.overflow.scrollWidth <= result.app.overflow.clientWidth,
        `folder picker created AppWindow horizontal overflow: ${JSON.stringify(result)}`);
      assert.ok(result.dialog.overflow.scrollWidth <= result.dialog.overflow.clientWidth,
        `folder dialog created horizontal overflow: ${JSON.stringify(result.dialog)}`);
      assert.ok(result.dialog.rect.left >= result.backdrop.left - 0.1);
      assert.ok(result.dialog.rect.right <= result.backdrop.right + 0.1);
      assert.ok(result.dialog.rect.top >= result.backdrop.top - 0.1);
      assert.ok(result.dialog.rect.bottom <= result.backdrop.bottom + 0.1);
      assert.ok(result.main.bottom <= result.footer.top + 0.1,
        `folder contents overlaid the selection footer: ${JSON.stringify({ main: result.main, footer: result.footer, dialog: result.dialog })}`);
      assert.ok(result.list.rect.bottom <= result.footer.top + 0.1,
        `scrolling folder list escaped below the footer: ${JSON.stringify({ list: result.list, footer: result.footer, main: result.main, dialog: result.dialog })}`);
      assert.ok(result.list.rect.bottom <= result.main.bottom - 6.5,
        `scrolling folder list escaped the padded content pane: ${JSON.stringify({ list: result.list, main: result.main })}`);
      assert.ok(result.list.overflow.clientHeight >= 54,
        `folder explorer did not preserve one visible folder row: ${JSON.stringify(result)}`);
      assert.ok(result.list.overflow.scrollWidth <= result.list.overflow.clientWidth,
        "long folder paths caused horizontal list overflow");
      assert.ok(result.list.overflow.scrollHeight > result.list.overflow.clientHeight,
        "folder fixture did not prove independent vertical scrolling");
      assert.equal(result.list.overflow.overflowX, "hidden");
      assert.equal(result.list.overflow.overflowY, "auto");
      assert.equal(result.openDisplay, "grid", "folder rows must retain explorer columns instead of generic button layout");
      assert.equal(result.breadcrumbs.overflow.overflowX, "auto");
      assert.ok(result.toolbar.overflow.scrollWidth <= result.toolbar.overflow.clientWidth,
        "toolbar escaped its compact picker width");
    }
    assert.ok(narrow.breadcrumbs.overflow.scrollWidth > narrow.breadcrumbs.overflow.clientWidth,
      "long compact breadcrumbs did not remain independently horizontally scrollable");
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});
