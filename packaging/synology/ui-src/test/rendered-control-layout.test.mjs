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
const baselineCss = css.slice(0, css.indexOf("@container (max-width: 720px)"));
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

function renderAttempt(chrome, url, profileDirectory) {
  return spawnSync(chrome, [
    "--headless=new",
    "--disable-gpu",
    "--disable-dev-shm-usage",
    "--no-sandbox",
    `--user-data-dir=${profileDirectory}`,
    "--window-size=1100,1000",
    "--virtual-time-budget=500",
    "--dump-dom",
    url
  ], {
    encoding: "utf8",
    maxBuffer: 8 * 1024 * 1024,
    timeout: 45000,
    windowsHide: true
  });
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

function render(chrome, url, profileDirectory) {
  const attempts = [renderAttempt(chrome, url, profileDirectory)];
  if (shouldRetryRender(attempts[0])) {
    attempts.push(renderAttempt(chrome, url, `${profileDirectory}-retry`));
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
      }
      .dsm-host .dsm-checkbox,
      .dsm-host .dsm-checkbox label {
        display: block !important;
        width: 160% !important;
        max-width: none !important;
        min-width: 440px !important;
        margin: 20px !important;
        padding: 16px !important;
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
      <style>${hostileCss}\n${baselineCss}</style>
      <body class="dsm-host">
        <div class="sdsync-app" style="display:block !important; width:100% !important; min-height:0 !important; overflow:visible !important">
          <div id="window-shell">
            <form id="settings-panel" class="sdsync-settings-panel">
              <div id="select-row" class="dsm-form-item sdsync-form-item">
                <div id="select-control-shell" class="dsm-private-control-shell">
                  <div id="form-select-root" class="dsm-select sdsync-select-control">
                    <span class="dsm-owned-decoration" aria-hidden="true"></span>
                    <div id="form-select-shell-one" class="dsm-private-select-shell">
                      <div id="form-select-shell-two" class="dsm-private-select-shell">
                        <div id="form-combobox" role="combobox" aria-haspopup="listbox">
                          <input id="form-select-input" value="System default">
                          <button id="form-select-trigger" type="button" aria-label="Open options"><svg class="sdsync-action-icon" viewBox="0 0 12 8"><path d="M1 1l5 5 5-5" /></svg></button>
                        </div>
                      </div>
                    </div>
                  </div>
                </div>
                <div id="select-label-shell" class="dsm-private-label"><label>Theme</label></div>
              </div>
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
          </div>
          <div id="check-row" class="sdsync-check-row" style="max-width:680px">
            <div id="checkbox-root" class="dsm-checkbox sdsync-checkbox-control">
              <span id="checkbox-decoration" class="dsm-owned-decoration" aria-hidden="true"></span>
              <div id="checkbox-shell-one" class="dsm-private-checkbox-shell">
                <div id="checkbox-shell-two" class="dsm-private-checkbox-shell">
                  <label id="checkbox-label"><input id="checkbox" type="checkbox"><span id="checkbox-text">Enable package operation</span></label>
                </div>
              </div>
            </div>
            <span id="checkbox-help" class="sdsync-field-tip"><button type="button" class="sdsync-field-tip-trigger" aria-label="Help">?</button><span class="sdsync-field-tip-content" role="tooltip">Checkbox help</span></span>
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
              marginTop: value.marginTop,
              paddingTop: value.paddingTop,
              minWidth: value.minWidth,
              maxWidth: value.maxWidth
            };
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
            formSelect: selectVariant("form-select-root", "form-combobox", "form-select-input", "form-select-trigger", ["form-select-shell-one", "form-select-shell-two"]),
            inputRoot: { rect: rect("input-root"), style: style("input-root") },
            inputShells: [rect("input-shell-one"), rect("input-shell-two")],
            textInput: rect("text-input"),
            checkRow: rect("check-row"),
            checkboxRoot: { rect: rect("checkbox-root"), style: style("checkbox-root") },
            checkboxShells: [rect("checkbox-shell-one"), rect("checkbox-shell-two")],
            checkboxLabel: { rect: rect("checkbox-label"), style: style("checkbox-label") },
            checkbox: rect("checkbox"),
            checkboxText: rect("checkbox-text"),
            checkboxHelp: rect("checkbox-help"),
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
    const narrow = render(chrome, `${url}?container=640`, join(temporaryDirectory, "profile-narrow"));

    for (const form of [wide.selectForm, wide.inputForm]) {
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

    for (const select of [wide.formSelect, ...wide.variants]) {
      assert.equal(select.root.style.display, "inline-flex");
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

    assert.equal(wide.inputRoot.style.display, "inline-flex");
    assert.ok(wide.textInput.width > 200, "nested text input collapsed");
    assert.ok(wide.textInput.right <= wide.inputRoot.rect.right + 0.1, "nested text input overflowed its owned root");
    for (const shell of wide.inputShells) {
      assert.ok(shell.right <= wide.inputRoot.rect.right + 0.1, "nested private input shell overflowed its root");
    }

    assert.match(wide.checkboxRoot.style.display, /^(?:inline-)?flex$/);
    assert.match(wide.checkboxLabel.style.display, /^(?:inline-)?flex$/);
    assert.ok(overlapsVertically(wide.checkbox, wide.checkboxText), "checkbox and label text stacked vertically");
    assert.ok(wide.checkboxRoot.rect.width <= wide.checkRow.width + 0.1, "checkbox root overflowed its owned grid track");
    assert.ok(wide.checkboxRoot.rect.right <= wide.checkboxHelp.left + 0.1, "checkbox label overlapped its help control");
    assert.ok(
      wide.checkboxHelp.right <= wide.checkRow.right + 0.1,
      `checkbox help escaped its owned row: ${JSON.stringify({ row: wide.checkRow, root: wide.checkboxRoot, help: wide.checkboxHelp })}`
    );
    for (const shell of wide.checkboxShells) {
      assert.ok(shell.right <= wide.checkboxRoot.rect.right + 0.1, "nested private checkbox shell overflowed its root");
    }

    assert.ok(wide.viewport > 720 && narrow.viewport > 720, "fixture must keep the DSM browser viewport wide");
    assert.ok(wide.container.width > 720, `wide AppWindow unexpectedly narrow: ${wide.container.width}`);
    assert.ok(narrow.container.width <= 720, `narrow AppWindow missed its container query: ${narrow.container.width}`);
    for (const form of [narrow.selectForm, narrow.inputForm]) {
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
