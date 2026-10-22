import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const sourceCssUrl = new URL("../src/styles/native.css", import.meta.url);
const distCssUrl = new URL("../dist/style.css", import.meta.url);
const appSourceUrl = new URL("../src/App.vue", import.meta.url);

function selectorBlock(css, selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = css.match(new RegExp(`(?:^|\\n)${escaped}\\s*\\{([^}]*)\\}`));
  assert.ok(match, `missing ${selector} CSS rule`);
  return match[1];
}

function assertSolidToastContract(css, label) {
  const toast = selectorBlock(css, ".sdsync-toast");
  assert.match(toast, /(?:^|\n)\s*isolation:\s*isolate\s*;/);
  assert.match(toast, /(?:^|\n)\s*opacity:\s*1\s*!important\s*;/);
  assert.match(
    toast,
    /(?:^|\n)\s*background-color:\s*var\(--sdsync-toast-surface\)\s*!important\s*;/,
  );
  assert.match(toast, /(?:^|\n)\s*background-image:\s*none\s*!important\s*;/);

  const errorToast = selectorBlock(css, ".sdsync-toast.is-error");
  assert.match(
    errorToast,
    /(?:^|\n)\s*background-color:\s*var\(--sdsync-toast-error-surface\)\s*!important\s*;/,
  );

  for (const token of ["sdsync-toast-surface", "sdsync-toast-error-surface"]) {
    const values = [
      ...css.matchAll(new RegExp(`--${token}:\\s*([^;]+);`, "g")),
    ].map((match) => match[1].trim());
    assert.equal(values.length, 2, `${label} must define dark and light ${token}`);
    for (const value of values) {
      assert.match(value, /^#[0-9a-f]{6}$/i, `${label} ${token} must be an opaque hex color`);
    }
  }
}

function assertLiveOperationContract(css, label) {
  const surface = selectorBlock(css, ".sdsync-live-operation");
  assert.match(surface, /(?:^|\n)\s*position:\s*absolute\s*;/);
  assert.match(surface, /(?:^|\n)\s*top:\s*calc\(100% \+ 8px\)\s*;/);
  assert.match(surface, /(?:^|\n)\s*right:\s*0\s*;/);
  assert.match(surface, /(?:^|\n)\s*display:\s*grid\s*;/);
  assert.match(surface, /(?:^|\n)\s*opacity:\s*1\s*!important\s*;/);
  assert.match(
    surface,
    /(?:^|\n)\s*background-color:\s*var\(--sdsync-panel-strong\)\s*!important\s*;/,
  );
  assert.match(surface, /(?:^|\n)\s*background-image:\s*none\s*!important\s*;/);
  assert.match(surface, /(?:^|\n)\s*pointer-events:\s*none\s*;/);
  assert.match(surface, /(?:^|\n)\s*max-width:\s*calc\(100% - 36px\)\s*;/);

  const topbar = selectorBlock(css, ".sdsync-topbar");
  assert.match(topbar, /(?:^|\n)\s*position:\s*sticky\s*;/);
  assert.doesNotMatch(topbar, /(?:^|\n)\s*overflow:\s*hidden\s*;/);

  const indicator = selectorBlock(css, ".sdsync-live-operation-indicator");
  assert.match(indicator, /(?:^|\n)\s*display:\s*grid\s*;/);
  assert.match(indicator, /(?:^|\n)\s*color:\s*var\(--sdsync-fire\)\s*;/);

  const panelValues = [
    ...css.matchAll(/--sdsync-panel-strong:\s*([^;]+);/g),
  ].map((match) => match[1].trim());
  assert.ok(panelValues.length >= 2, `${label} must define dark and light live-operation surfaces`);
  for (const value of panelValues) {
    assert.match(value, /^#[0-9a-f]{6}$/i, `${label} live-operation surface must be an opaque hex color`);
  }
}

test("toast surfaces stay opaque despite host DSM styling", async () => {
  const [sourceCss, distCss] = await Promise.all([
    readFile(sourceCssUrl, "utf8"),
    readFile(distCssUrl, "utf8"),
  ]);
  assertSolidToastContract(sourceCss, "source CSS");
  assertSolidToastContract(distCss, "built CSS");
});

/**
 * Dashboard review #5. The toast container's `aria-live="polite"` region
 * used to wrap toast items that each also carried an implicit live region of
 * their own (role="alert" is assertive, role="status" is polite) -- nesting
 * live regions like that announces an error toast at the wrong level, or
 * twice, depending on the screen reader. Pick one: the container's, since
 * `<div class="sdsync-toasts" aria-live="polite"` is itself a structural
 * marker validate_spk.py pins for the native DSM AppWindow's toast host.
 */
test("the toast region has exactly one live-region mechanism: the container's aria-live, not a second one per toast", async () => {
  const app = await readFile(appSourceUrl, "utf8");
  const container = app.match(/<div class="sdsync-toasts"[^>]*>/);
  assert.ok(container, "toast container markup is missing");
  assert.match(
    container[0],
    /aria-live="polite"/,
    "validate_spk.py pins this exact prefix as the native DSM AppWindow's toast host"
  );
  const toastItem = app.match(/<div v-for="toastItem in toasts"[^>]*>/);
  assert.ok(toastItem, "toast item markup is missing");
  assert.doesNotMatch(
    toastItem[0],
    /role=/,
    "a per-toast role would be a second, nested live region beside the container's"
  );
  // The visual distinction between an error and an ordinary toast must
  // survive losing its ARIA mechanism -- is-error still drives the CSS.
  assert.match(toastItem[0], /'is-error': toastItem\.error/);
});

test("profile progress is an opaque visual duplicate of the in-form live status", async () => {
  const [appSource, sourceCss, distCss] = await Promise.all([
    readFile(appSourceUrl, "utf8"),
    readFile(sourceCssUrl, "utf8"),
    readFile(distCssUrl, "utf8"),
  ]);

  assert.match(
    appSource,
    /<header class="sdsync-topbar">[\s\S]*v-if="profileLiveOperation" class="sdsync-live-operation" aria-hidden="true"[\s\S]*<\/header>/,
  );
  assert.match(
    appSource,
    /class="sdsync-live-operation-indicator"><action-icon class="sdsync-is-spinning" name="refresh" size="22"/,
  );
  assert.match(appSource, /class="sdsync-live-operation-stage">\{\{ profileLiveOperation\.stage \}\}/);
  assert.match(appSource, /class="sdsync-live-operation-warning">\{\{ profileLiveOperation\.warning \}\}/);
  assert.match(appSource, /Keep this AppWindow open; do not navigate away until profile creation finishes\./);
  assert.doesNotMatch(appSource, /class="sdsync-live-operation"[^>]*(?:role="status"|aria-live=)/);
  assert.match(appSource, /:class="\['sdsync-connection-state'[^>]*role="status" aria-live="polite"/);
  assert.match(appSource, /:class="\['sdsync-save-state'[^>]*role="status" aria-live="polite"/);
  assertLiveOperationContract(sourceCss, "source CSS");
  assertLiveOperationContract(distCss, "built CSS");
});
