import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const app = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const css = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");

function rule(selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = css.match(new RegExp(`${escaped}\\s*\\{([\\s\\S]*?)\\}`));
  assert.ok(match, `missing ${selector} rule`);
  return match[1];
}

test("connection status is a semantic, live, isolated sidebar footer", () => {
  assert.match(
    app,
    /<footer class="sdsync-sidebar-foot" aria-label="Package connection status">[\s\S]*?<span aria-live="polite">\{\{ connectionLabel \}\}<\/span>[\s\S]*?<\/footer>/
  );

  const sidebar = rule(".sdsync-sidebar");
  assert.match(sidebar, /position:\s*relative/);
  assert.match(sidebar, /isolation:\s*isolate/);
  assert.match(sidebar, /overflow:\s*hidden/);
  assert.match(sidebar, /background-color:\s*var\(--sdsync-sidebar\)/);

  const footer = rule(".sdsync-sidebar-foot");
  assert.match(footer, /position:\s*relative/);
  assert.match(footer, /z-index:\s*[1-9]\d*/);
  assert.match(footer, /flex:\s*0\s+0\s+auto/);
  assert.match(footer, /border-top:\s*1px\s+solid\s+var\(--sdsync-border-soft\)/);
  assert.match(footer, /background-color:\s*var\(--sdsync-sidebar\)/);
  assert.match(footer, /box-shadow:\s*none/);
  assert.doesNotMatch(footer, /rgba?\(/, "the footer backing must be fully opaque");
});

test("navigation owns the short-viewport scroll region without clipping keyboard focus", () => {
  const brand = rule(".sdsync-brand");
  assert.match(brand, /flex:\s*0\s+0\s+auto/);

  const navigation = rule(".sdsync-nav");
  assert.match(navigation, /flex:\s*1\s+1\s+auto/);
  assert.match(navigation, /min-height:\s*0/);
  assert.match(navigation, /overflow-y:\s*auto/);
  assert.match(navigation, /overscroll-behavior:\s*contain/);
  assert.match(navigation, /scrollbar-gutter:\s*stable/);
  const padding = Number(navigation.match(/padding:\s*(\d+)px/)?.[1]);
  const negativeMargin = Number(navigation.match(/margin:\s*-(\d+)px/)?.[1]);
  assert.equal(padding, 5, "scroll padding must fully contain the last item's focus outline");
  assert.equal(negativeMargin, padding, "focus padding must not widen the sidebar layout");

  const focusRules = [...css.matchAll(/\.sdsync-nav-item:focus-visible\s*\{([\s\S]*?)\}/g)];
  assert.ok(focusRules.length, "navigation needs an authored keyboard focus state");
  const focus = focusRules.map((match) => match[1]).join("\n");
  assert.match(focus, /outline:\s*2px\s+solid\s+var\(--sdsync-focus\)/);
  assert.match(focus, /outline-offset:\s*2px/);
  const outlineWidth = Number(focus.match(/outline:\s*(\d+)px/)?.[1]);
  const outlineOffset = Number(focus.match(/outline-offset:\s*(\d+)px/)?.[1]);
  assert.ok(
    padding >= outlineWidth + outlineOffset,
    "the scroll region must reserve the full focus extent around its last item"
  );
  assert.doesNotMatch(
    rule(".sdsync-sidebar-foot"),
    /box-shadow:\s*[^;]*\s-\d+px/,
    "the footer must not paint upward across the final navigation item"
  );
});

test("toast surfaces are opaque, legible, distinct, and remain above AppWindow content", () => {
  const darkTheme = rule(".sdsync-app");
  const lightTheme = rule(".sdsync-app.is-light");
  for (const theme of [darkTheme, lightTheme]) {
    assert.match(theme, /--sdsync-toast-surface:\s*#[0-9a-f]{6}/i);
    assert.match(theme, /--sdsync-toast-error-surface:\s*#[0-9a-f]{6}/i);
    assert.doesNotMatch(
      theme.match(/--sdsync-toast-(?:error-)?surface:[^;]+/gi)?.join("\n") || "",
      /rgba?\(|hsla?\(|transparent/i
    );
  }

  const stack = rule(".sdsync-toasts");
  assert.match(stack, /position:\s*absolute/);
  assert.match(stack, /z-index:\s*20/);
  assert.match(stack, /pointer-events:\s*none/);

  const toast = rule(".sdsync-toast");
  const errorToast = rule(".sdsync-toast.is-error");
  assert.match(toast, /color:\s*var\(--sdsync-text\)/);
  assert.match(toast, /background-color:\s*var\(--sdsync-toast-surface\)/);
  assert.match(errorToast, /background-color:\s*var\(--sdsync-toast-error-surface\)/);
  assert.notEqual(
    toast.match(/background-color:[^;]+/i)?.[0],
    errorToast.match(/background-color:[^;]+/i)?.[0]
  );

  const reducedMotion = css.slice(css.indexOf("@media (prefers-reduced-motion: reduce)"));
  assert.match(reducedMotion, /\.sdsync-app \*/);
  assert.match(reducedMotion, /transition-duration:\s*0\.01ms\s*!important/);
  assert.match(reducedMotion, /animation-duration:\s*0\.01ms\s*!important/);
});
