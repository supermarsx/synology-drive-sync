import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const sourceCssUrl = new URL("../src/styles/native.css", import.meta.url);
const distCssUrl = new URL("../dist/style.css", import.meta.url);

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

test("toast surfaces stay opaque despite host DSM styling", async () => {
  const [sourceCss, distCss] = await Promise.all([
    readFile(sourceCssUrl, "utf8"),
    readFile(distCssUrl, "utf8"),
  ]);
  assertSolidToastContract(sourceCss, "source CSS");
  assertSolidToastContract(distCss, "built CSS");
});
