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

test("profile progress is an opaque floating live region with a visible spinner", async () => {
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
  assert.doesNotMatch(appSource, /class="sdsync-live-operation"[^>]*(?:role="status"|aria-live=)/);
  assert.match(appSource, /:class="\['sdsync-connection-state'[^>]*role="status" aria-live="polite"/);
  assert.match(appSource, /:class="\['sdsync-save-state'[^>]*role="status" aria-live="polite"/);
  assertLiveOperationContract(sourceCss, "source CSS");
  assertLiveOperationContract(distCss, "built CSS");
});
