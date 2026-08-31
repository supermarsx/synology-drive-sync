import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import test from "node:test";
import { runInNewContext } from "node:vm";

const iconSourceUrl = new URL("../src/ActionIcon.js", import.meta.url);
const appSourceUrl = new URL("../src/App.vue", import.meta.url);
const jsDistUrl = new URL("../dist/SynologyDriveSync.js", import.meta.url);
const cssSourceUrl = new URL("../src/styles/native.css", import.meta.url);
const cssDistUrl = new URL("../dist/style.css", import.meta.url);

async function loadActionIcon() {
  const source = await readFile(iconSourceUrl, "utf8");
  return import(`data:text/javascript;base64,${Buffer.from(source).toString("base64")}#${Date.now()}-${Math.random()}`);
}

async function loadBundledActionIcon() {
  const source = await readFile(jsDistUrl, "utf8");
  const extensions = [];
  const styleNodes = new Map();
  const document = {
    head: {
      appendChild(node) {
        if (node.id) styleNodes.set(node.id, node);
      }
    },
    documentElement: { appendChild() {} },
    getElementById(id) { return styleNodes.get(id) || null; },
    createElement(tag) {
      return { tag, id: "", type: "", textContent: "", setAttribute() {} };
    }
  };
  const Vue = {
    extend(options) {
      extensions.push(options);
      return options;
    }
  };
  const SYNO = {
    SDS: { App: { SynologyDriveSync: {} } },
    namespace(name) {
      assert.equal(name, "SYNO.SDS.App.SynologyDriveSync");
    }
  };
  runInNewContext(source, { Vue, SYNO, document }, {
    filename: "dist/SynologyDriveSync.js",
    timeout: 5000
  });
  assert.equal(extensions.length, 1, "built bundle must register one DSM Vue root class");
  const app = extensions[0].components && extensions[0].components.App;
  assert.ok(app && app.components, "built bundle did not expose the rendered App component graph");
  assert.ok(app.components.ActionIcon, "built App component is missing ActionIcon");
  return { ActionIcon: app.components.ActionIcon, source };
}

function renderIcon(ActionIcon, name, dynamicClass) {
  const createElement = (tag, data, children = []) => ({ tag, data, children });
  return ActionIcon.render(createElement, {
    props: { name, size: 22 },
    data: {
      class: dynamicClass,
      attrs: { "data-contract": "source-dist" },
      props: { name, size: 22 }
    }
  });
}

function portableVNode(node) {
  if (node === null || node === undefined || typeof node !== "object") return node;
  return {
    tag: node.tag,
    data: JSON.parse(JSON.stringify(node.data || {})),
    children: Array.from(node.children || [], portableVNode)
  };
}

function ruleAfter(css, marker) {
  const start = css.indexOf(marker);
  assert.notEqual(start, -1, `missing CSS marker ${marker}`);
  const open = css.indexOf("{", start);
  const close = css.indexOf("}", open);
  assert.ok(open > start && close > open, `missing CSS block after ${marker}`);
  return css.slice(open + 1, close);
}

function assertSpinnerCss(css, label) {
  const buttonIcons = ruleAfter(
    css,
    ".sdsync-app button:not(.sdsync-nav-item):not(.sdsync-profile-row):not(.sdsync-routine-row) > .sdsync-action-icon,"
  );
  assert.match(buttonIcons, /width:\s*15px\s*!important/);
  assert.match(buttonIcons, /height:\s*15px\s*!important/);
  assert.match(buttonIcons, /flex:\s*0 0 15px\s*!important/);
  assert.doesNotMatch(
    buttonIcons,
    /transform:\s*none\s*!important/,
    `${label} button normalization must not suppress SVG transform animation`,
  );

  const spinner = ruleAfter(css, ".sdsync-app .sdsync-action-icon.sdsync-is-spinning > .sdsync-action-icon-glyph");
  assert.match(spinner, /animation:\s*sdsync-spin 0\.8s linear infinite !important/);
  assert.match(spinner, /transform-origin:\s*12px 12px !important/);
  assert.match(spinner, /transform-box:\s*view-box/);
  assert.match(spinner, /will-change:\s*transform/);
  assert.match(css, /@keyframes sdsync-spin\s*\{\s*from\s*\{\s*transform:\s*rotate\(0deg\)/);
  assert.match(css, /to\s*\{\s*transform:\s*rotate\(360deg\)/);

  const reducedStart = css.indexOf("@media (prefers-reduced-motion: reduce)");
  assert.notEqual(reducedStart, -1, `${label} lacks a reduced-motion contract`);
  const reduced = css.slice(reducedStart);
  assert.match(
    reduced,
    /\.sdsync-app \.sdsync-action-icon\.sdsync-is-spinning\s*\{[^}]*transform:\s*none\s*!important[^}]*stroke-dasharray:\s*3 2/s,
  );
  assert.match(
    reduced,
    /\.sdsync-app \.sdsync-action-icon\.sdsync-is-spinning > \.sdsync-action-icon-glyph\s*\{[^}]*animation:\s*sdsync-busy-pulse 1\.6s ease-in-out infinite\s*!important[^}]*will-change:\s*opacity/s,
  );
  assert.match(
    reduced,
    /\.sdsync-live-operation-indicator\s*\{[^}]*border-style:\s*dashed[^}]*box-shadow:\s*inset 3px 0 0 var\(--sdsync-fire\)/s,
  );
}

test("functional ActionIcon forwards caller classes and presentation data", async () => {
  const { ActionIcon } = await loadActionIcon();
  const nodes = [];
  const createElement = (tag, data, children = []) => {
    const node = { tag, data, children };
    nodes.push(node);
    return node;
  };
  const dynamicClass = { "sdsync-is-spinning": true, "is-muted": false };
  const dynamicStyle = { opacity: 0.75 };
  const rendered = ActionIcon.render(createElement, {
    props: { name: "refresh", size: 22 },
    data: {
      staticClass: "caller-static",
      class: dynamicClass,
      staticStyle: { color: "red" },
      style: dynamicStyle,
      attrs: { "data-contract": "forwarded", width: 999 },
      on: { click() {} },
      props: { name: "refresh", size: 22 }
    }
  });

  assert.equal(rendered.tag, "svg");
  assert.equal(rendered.data.staticClass, "caller-static");
  assert.deepEqual(rendered.data.class, ["sdsync-action-icon", dynamicClass]);
  assert.deepEqual(rendered.data.staticStyle, { color: "red" });
  assert.strictEqual(rendered.data.style[1], dynamicStyle);
  assert.equal(rendered.data.attrs["data-contract"], "forwarded");
  assert.equal(rendered.data.attrs.width, 22, "component size remains authoritative");
  assert.equal(rendered.data.attrs["aria-hidden"], "true");
  assert.equal(rendered.data.props, undefined, "component-only props must not leak onto the SVG");
  assert.equal(typeof rendered.data.on.click, "function");
  assert.equal(rendered.children.length, 1);
  assert.equal(rendered.children[0].tag, "g");
  assert.equal(rendered.children[0].data.class, "sdsync-action-icon-glyph");
  assert.ok(nodes.filter((node) => node.tag === "path").length >= 2);

  const staticRendered = ActionIcon.render(createElement, {
    props: { name: "refresh", size: 16 },
    data: { attrs: {}, props: { name: "refresh", size: 16 } }
  });
  assert.deepEqual(staticRendered.data.class, ["sdsync-action-icon", undefined]);
  assert.equal(staticRendered.children[0].data.class, "sdsync-action-icon-glyph");
});

test("actual built bundle renders the same isolated ActionIcon glyph wrapper as source", async () => {
  const [{ ActionIcon: sourceIcon }, { ActionIcon: builtIcon }] = await Promise.all([
    loadActionIcon(),
    loadBundledActionIcon()
  ]);
  for (const name of ["refresh", "copy"]) {
    const dynamicClass = { "sdsync-is-spinning": name === "refresh" };
    const sourceRendered = portableVNode(renderIcon(sourceIcon, name, dynamicClass));
    const builtRendered = portableVNode(renderIcon(builtIcon, name, dynamicClass));
    assert.deepEqual(builtRendered, sourceRendered, `${name} differs between ActionIcon source and the built component graph`);
    assert.equal(builtRendered.children.length, 1);
    assert.equal(builtRendered.children[0].tag, "g");
    assert.equal(builtRendered.children[0].data.class, "sdsync-action-icon-glyph");
    assert.ok(builtRendered.children[0].children.length >= 2);
  }
});

test("snapshot Refresh and Retry bind their built icons to snapshotLoading", async () => {
  const [app, bundle] = await Promise.all([
    readFile(appSourceUrl, "utf8"),
    readFile(jsDistUrl, "utf8")
  ]);
  const sourceBindings = app.match(/<action-icon :class="\{ 'sdsync-is-spinning': snapshotLoading \}" name="refresh"/g) || [];
  assert.equal(sourceBindings.length, 2, "Refresh and Retry must both bind their icon animation to snapshotLoading");
  const builtBindings = bundle.match(/class:\{"sdsync-is-spinning":[A-Za-z_$][\w$]*\.snapshotLoading\}/g) || [];
  assert.equal(builtBindings.length, 2, "the built render functions must preserve both snapshotLoading icon bindings");
});

test("source and built CSS keep busy icons rotating without layout shift", async () => {
  const [source, dist] = await Promise.all([
    readFile(cssSourceUrl, "utf8"),
    readFile(cssDistUrl, "utf8")
  ]);
  assertSpinnerCss(source, "source CSS");
  assertSpinnerCss(dist, "built CSS");
});

function browserCandidate() {
  const configured = [process.env.CHROME_BIN, process.env.GOOGLE_CHROME_BIN].filter(Boolean);
  const candidates = process.platform === "win32"
    ? configured.concat([
      "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
      "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
      "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe"
    ])
    : configured.concat(["google-chrome-stable", "google-chrome", "chromium", "chromium-browser"]);
  for (const candidate of candidates) {
    if ((candidate.includes("/") || candidate.includes("\\")) && !existsSync(candidate)) continue;
    const probe = spawnSync(candidate, ["--version"], { encoding: "utf8", timeout: 5000, windowsHide: true });
    if (!probe.error && probe.status === 0) return candidate;
  }
  return null;
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
    "--no-first-run",
    "--no-default-browser-check",
    "--no-sandbox",
    `--user-data-dir=${profileDirectory}`,
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
  const missingEvidence = !/data-spinner="[A-Za-z0-9+/=]+"/.test(result.stdout || "");
  return Boolean(timedOut || missingEvidence);
}

function renderSpinner(chrome, url, profileDirectory, { retry = true, timeoutMs = 45000 } = {}) {
  const attempts = [renderAttempt(chrome, url, profileDirectory, timeoutMs)];
  if (retry && shouldRetryRender(attempts[0])) {
    attempts.push(renderAttempt(chrome, url, `${profileDirectory}-retry`, timeoutMs));
  }
  const result = attempts[attempts.length - 1];
  const diagnostics = JSON.stringify(attempts.map(renderAttemptSummary));
  assert.equal(result.status, 0, `headless browser failed after ${attempts.length} bounded attempt(s): ${diagnostics}`);
  assert.match(result.stdout || "", /data-spinner="[A-Za-z0-9+/=]+"/,
    `headless browser returned no computed spinner evidence after ${attempts.length} bounded attempt(s): ${diagnostics}`);
  return result.stdout;
}

function browserProbePolicy({ chromeAvailable, ci, platform, arch, gate = "" }) {
  if (!["", "required", "skip-synology-architecture-matrix"].includes(gate)) {
    throw new Error(`unsupported SDSYNC_UI_BROWSER_GATE value: ${gate}`);
  }
  if (gate === "skip-synology-architecture-matrix" && ci) {
    return {
      mandatory: false,
      skipReason: "Reviewed skip: computed animation is authoritative in the general x64 packaging job"
    };
  }
  if (gate === "required") return { mandatory: true, skipReason: "" };
  if (chromeAvailable) return { mandatory: true, skipReason: "" };
  if (!ci) {
    return {
      mandatory: false,
      skipReason: "Chrome/Chromium is unavailable for the computed-animation probe"
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

const browserGate = process.env.SDSYNC_UI_BROWSER_GATE || "";
const runningInCi = /^(?:1|true)$/i.test(process.env.CI || "");
const reviewedMatrixSkip = browserGate === "skip-synology-architecture-matrix" && runningInCi;
const chrome = reviewedMatrixSkip ? null : browserCandidate();
const browserPolicy = browserProbePolicy({
  chromeAvailable: Boolean(chrome),
  ci: runningInCi,
  platform: process.platform,
  arch: process.arch,
  gate: browserGate
});

test("computed-animation browser gate has one authoritative CI lane and reviewed matrix skips", () => {
  assert.equal(browserProbePolicy({
    chromeAvailable: true,
    ci: true,
    platform: "linux",
    arch: "x64",
    gate: "skip-synology-architecture-matrix"
  }).mandatory, false, "Synology architecture jobs skip the redundant probe even when Chrome is present");
  assert.equal(browserProbePolicy({
    chromeAvailable: false,
    ci: true,
    platform: "linux",
    arch: "x64",
    gate: "required"
  }).mandatory, true, "the general x64 packaging gate must fail if its browser disappears");
  assert.throws(
    () => browserProbePolicy({ chromeAvailable: true, ci: true, platform: "linux", arch: "x64", gate: "skip" }),
    /unsupported SDSYNC_UI_BROWSER_GATE/,
    "an unreviewed workflow value cannot silently disable the animation gate"
  );
  assert.equal(shouldRetryRender({ error: { code: "ETIMEDOUT" }, stdout: "" }), true);
  assert.equal(shouldRetryRender({ error: null, stdout: "" }), true);
  assert.equal(shouldRetryRender({ error: null, stdout: 'data-spinner="e30="' }), false);
});

test("browser applies busy animation to the glyph while leaving a static icon untouched", {
  skip: browserPolicy.skipReason || false
}, async () => {
  assert.ok(chrome, `Chrome/Chromium is required on ${process.platform}/${process.arch} CI`);
  const directory = await mkdtemp(join(tmpdir(), "sdsync-spinner-"));
  try {
    const css = await readFile(cssSourceUrl, "utf8");
    const html = `<!doctype html><html><head><meta charset="utf-8"><style>${css}</style></head><body>
      <div class="sdsync-app is-dark">
        <svg id="busy" class="sdsync-action-icon sdsync-is-spinning" viewBox="0 0 24 24"><g class="sdsync-action-icon-glyph"><path d="M20 6v5h-5"></path><path d="M4 18v-5h5"></path></g></svg>
        <svg id="static" class="sdsync-action-icon" viewBox="0 0 24 24"><g class="sdsync-action-icon-glyph"><path d="M20 6v5h-5"></path></g></svg>
      </div>
      <script>
        const busyGlyph = document.querySelector('#busy > .sdsync-action-icon-glyph');
        const staticGlyph = document.querySelector('#static > .sdsync-action-icon-glyph');
        const animation = busyGlyph.getAnimations()[0];
        const result = {
          reduced: matchMedia('(prefers-reduced-motion: reduce)').matches,
          busyAnimationName: getComputedStyle(busyGlyph).animationName,
          busyAnimationCount: busyGlyph.getAnimations().length,
          staticAnimationName: getComputedStyle(staticGlyph).animationName,
          staticAnimationCount: staticGlyph.getAnimations().length,
          first: '', second: ''
        };
        if (animation) {
          animation.pause();
          animation.currentTime = 0;
          result.first = result.reduced ? getComputedStyle(busyGlyph).opacity : getComputedStyle(busyGlyph).transform;
          animation.currentTime = result.reduced ? 800 : 200;
          result.second = result.reduced ? getComputedStyle(busyGlyph).opacity : getComputedStyle(busyGlyph).transform;
        }
        document.body.setAttribute('data-spinner', btoa(JSON.stringify(result)));
      </script>
    </body></html>`;
    const htmlPath = join(directory, "spinner.html");
    await writeFile(htmlPath, html, "utf8");
    const profile = join(directory, "profile");
    const rendered = renderSpinner(chrome, pathToFileURL(htmlPath).href, profile);
    const match = rendered.match(/data-spinner="([A-Za-z0-9+/=]+)"/);
    const result = JSON.parse(Buffer.from(match[1], "base64").toString("utf8"));
    assert.equal(result.busyAnimationCount, 1);
    assert.equal(result.busyAnimationName, result.reduced ? "sdsync-busy-pulse" : "sdsync-spin");
    assert.notEqual(result.first, result.second, "the applied busy animation did not change rendered state");
    assert.equal(result.staticAnimationName, "none");
    assert.equal(result.staticAnimationCount, 0);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
