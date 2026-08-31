import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const css = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");
const app = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function declarations(selector) {
  const match = css.match(new RegExp(`${escapeRegex(selector)}\\s*\\{([\\s\\S]*?)\\n\\}`, "m"));
  assert.ok(match, `missing ${selector} declaration block`);
  return match[1];
}

function hexVariables(selector) {
  const variables = new Map();
  for (const match of declarations(selector).matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6})\s*;/gi)) {
    variables.set(match[1], match[2]);
  }
  return variables;
}

function luminance(hex) {
  const channels = [1, 3, 5].map((offset) => Number.parseInt(hex.slice(offset, offset + 2), 16) / 255);
  const linear = channels.map((value) => value <= 0.04045
    ? value / 12.92
    : ((value + 0.055) / 1.055) ** 2.4);
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}

function contrast(first, second) {
  const values = [luminance(first), luminance(second)].sort((left, right) => right - left);
  return (values[0] + 0.05) / (values[1] + 0.05);
}

function requireContrast(palette, foreground, background, minimum) {
  const foregroundColor = palette.get(`sdsync-${foreground}`);
  const backgroundColor = palette.get(`sdsync-${background}`);
  assert.ok(foregroundColor, `missing --sdsync-${foreground}`);
  assert.ok(backgroundColor, `missing --sdsync-${background}`);
  const ratio = contrast(foregroundColor, backgroundColor);
  assert.ok(
    ratio >= minimum,
    `${foreground} on ${background} contrast ${ratio.toFixed(2)} is below ${minimum}`
  );
}

test("dark-first and optional light palettes retain readable control contrast", () => {
  for (const [selector, name] of [[".sdsync-app", "dark"], [".sdsync-app.is-light", "light"]]) {
    const palette = hexVariables(selector);
    for (const surface of ["bg", "panel", "control"]) {
      requireContrast(palette, "text", surface, 7);
      requireContrast(palette, "muted", surface, 4.5);
    }
    requireContrast(palette, "placeholder", "control", 4.5);
    requireContrast(palette, "disabled", "control-disabled", 4.5);
    requireContrast(palette, "fire", "panel", 4.5);
    requireContrast(palette, "red", "panel", 4.5);
    requireContrast(palette, "amber", "panel", 4.5);
    requireContrast(palette, "on-accent", "primary", 4.5);
    requireContrast(palette, "on-danger", "danger", 4.5);
    requireContrast(palette, "control-border", "control", 3);
    requireContrast(palette, "focus", "control", 3);
    assert.match(declarations(selector), new RegExp(`color-scheme:\\s*${name}`));
  }
});

test("every native and DSM Vue control state is explicitly dark themed", () => {
  const requiredSelectors = [
    '.sdsync-app input:not([type="checkbox"]):not([type="radio"])',
    ".sdsync-app textarea",
    ".sdsync-app select",
    '.sdsync-app [role="combobox"]',
    '.sdsync-app [aria-haspopup="listbox"]',
    ".sdsync-app input::placeholder",
    ".sdsync-app input:disabled",
    '.sdsync-app [role="combobox"][aria-disabled="true"]',
    '.sdsync-app form [aria-invalid="true"]',
    ".sdsync-app input:not([type=\"checkbox\"]):not([type=\"radio\"]):focus-visible",
    ".sdsync-app .sdsync-input-control",
    ".sdsync-app .sdsync-select-control",
    ".sdsync-app .sdsync-control-owner.sdsync-input-control",
    ".sdsync-app .sdsync-control-owner.sdsync-select-control",
    ".sdsync-app .sdsync-control-owner.sdsync-input-control input.sdsync-semantic-control",
    ".sdsync-app .sdsync-control-owner.sdsync-input-control textarea.sdsync-semantic-control",
    '.sdsync-app [class*="syno"][class*="button"]',
    ".sdsync-app .sdsync-checkbox-control",
    ".sdsync-app .sdsync-toggle-row .sdsync-checkbox-input",
    ".sdsync-app .sdsync-toggle-row .sdsync-checkbox-input:checked",
    '.sdsync-app [role="listbox"]',
    '.sdsync-app [role="listbox"] [role="option"][aria-selected="true"]',
    '.sdsync-app [role="tooltip"]'
  ];
  for (const selector of requiredSelectors) assert.ok(css.includes(selector), `missing ${selector}`);

  for (const component of ["v-form", "v-form-item", "v-input", "v-single-select", "v-checkbox", "v-button"]) {
    assert.match(app, new RegExp(`<${component}\\b`), `AppWindow no longer exercises ${component}`);
  }
  assert.match(css, /-webkit-text-fill-color:\s*var\(--sdsync-disabled\)\s*!important/);
  assert.match(
    css,
    /input:not\(\[type="checkbox"\]\):not\(\[type="radio"\]\):focus-visible,[\s\S]*?textarea:focus-visible,[\s\S]*?select:focus-visible,[\s\S]*?\{[\s\S]*?border-color:\s*var\(--sdsync-focus\)\s*!important;[\s\S]*?outline:\s*(?:none|0)\s*!important;[\s\S]*?box-shadow:\s*none\s*!important;/,
    "form controls must replace the DSM border with one focus border, not stack an outer ring"
  );
  assert.match(
    css,
    /button:not\([^\n]+\):focus-visible,[\s\S]*?\{[\s\S]*?outline:\s*2px solid var\(--sdsync-focus\)\s*!important;/,
    "discrete buttons still need an external keyboard focus outline"
  );
  assert.match(css, /box-shadow:\s*0 0 0 3px rgba\(255, 107, 85, 0\.2\)\s*!important/);
});

test("tooltips, overlays, tables, and log surfaces stay readable inside the AppWindow", () => {
  for (const marker of [
    ".sdsync-app [role=\"tooltip\"]",
    ".sdsync-field-tip-trigger:focus-visible",
    ".sdsync-field-tip:hover .sdsync-field-tip-content",
    ".sdsync-field-tip:focus-within .sdsync-field-tip-content",
    "max-width: min(240px, calc(100vw - 96px))",
    "max-width: min(360px, calc(100vw - 24px))",
    "max-height: min(420px, calc(100vh - 24px))",
    "max-width: 320px",
    ".sdsync-toast.is-error",
    ".sdsync-modal:focus-visible",
    ".sdsync-app tbody tr:hover",
    ".sdsync-app pre:focus-visible",
    "scrollbar-color: var(--sdsync-control-border) var(--sdsync-control)"
  ]) {
    assert.ok(css.includes(marker), `missing surface theme contract ${marker}`);
  }
  assert.doesNotMatch(css, /(?:^|[},])\s*(?::root|html\b|body\b)/im);
  assert.doesNotMatch(css, /color-mix\(|url\(\s*["']?(?:https?:)?\/\//i);

  for (const line of css.split(/\r?\n/)) {
    const selector = line.trim();
    if (!selector.endsWith("{") || selector.startsWith("@")) continue;
    assert.match(selector, /^\.sdsync-/, `unscoped selector ${selector}`);
  }
});

test("PortalTarget select menus use a package-owned dark-theme boundary", () => {
  const selects = app.match(/<v-single-select\b[^>]*>/g) || [];
  assert.ok(selects.length > 0, "AppWindow no longer exercises portal-backed selects");
  for (const select of selects) {
    assert.match(
      select,
      /:custom-dropdown-cls="'sdsync-select-dropdown ' \+ themeClass"/,
      `select lacks the package-owned portal theme hook: ${select}`
    );
  }

  for (const selector of [".sdsync-select-dropdown", ".sdsync-select-dropdown.is-light"]) {
    const palette = hexVariables(selector);
    requireContrast(palette, "text", "control", 7);
    requireContrast(palette, "placeholder", "control", 4.5);
    requireContrast(palette, "disabled", "control-disabled", 4.5);
    requireContrast(palette, "control-border", "control", 3);
    requireContrast(palette, "focus", "control", 3);
    requireContrast(palette, "fire", "control", 4.5);
  }
  for (const marker of [
    '.sdsync-select-dropdown [role="listbox"]',
    '.sdsync-select-dropdown [role="option"][aria-selected="true"]',
    '.sdsync-select-dropdown [role="option"][aria-disabled="true"]',
    '.sdsync-select-dropdown input:focus-visible'
  ]) {
    assert.ok(css.includes(marker), `missing portal select contract ${marker}`);
  }
  const portal = declarations(".sdsync-select-dropdown");
  assert.match(portal, /max-width:\s*min\(360px, calc\(100vw - 24px\)\)\s*!important/);
  assert.match(portal, /max-height:\s*min\(420px, calc\(100vh - 24px\)\)\s*!important/);
  assert.match(portal, /overflow-x:\s*hidden\s*!important/);
  assert.match(declarations(".sdsync-workspace"), /overflow-x:\s*hidden/);
  assert.match(
    css,
    /\.sdsync-toggle-label > \.sdsync-field-tip \.sdsync-field-tip-content,[\s\S]*?inset-inline-start:\s*auto;[\s\S]*?inset-inline-end:\s*0;/
  );
});

test("security controls use compact responsive rows without clipping help", () => {
  for (const selector of [
    ".sdsync-security-form",
    ".sdsync-security-grid",
    ".sdsync-policy-list",
    ".sdsync-policy-control",
    ".sdsync-log-policy-grid",
    ".sdsync-security-actions"
  ]) {
    assert.ok(css.includes(`${selector} {`), `missing security layout ${selector}`);
  }
  assert.match(declarations(".sdsync-security-form"), /gap:\s*14px/);
  assert.match(declarations(".sdsync-security-grid"), /grid-template-columns:\s*repeat\(2, minmax\(0, 1fr\)\)/);
  assert.match(declarations(".sdsync-toggle-row"), /grid-template-columns:\s*minmax\(145px, 190px\) minmax\(44px, 1fr\)/);
  assert.match(declarations(".sdsync-toggle-row"), /width:\s*100%/);
  assert.match(declarations(".sdsync-toggle-row"), /overflow:\s*visible/);
  assert.match(declarations(".sdsync-policy-control"), /padding-block:\s*3px/);
  assert.match(declarations(".sdsync-log-policy-grid"), /grid-template-columns:\s*minmax\(0, 1fr\)/);
  assert.doesNotMatch(declarations(".sdsync-log-policy-grid"), /border-top:/,
    "the first security row owns the separator; its grid must not double it");
  assert.doesNotMatch(declarations(".sdsync-form-grid"), /border-top:/,
    "the first form row owns the separator; its grid must not double it");
  assert.match(declarations(".sdsync-security-actions"), /justify-content:\s*flex-end/);
  assert.match(css, /\.sdsync-security-grid\s*\{[\s\S]*?grid-template-columns:\s*1fr\s*;/);
  assert.match(css, /\.sdsync-log-policy-grid\s*\{[\s\S]*?grid-template-columns:\s*1fr\s*;/);
});

test("hellfire palette and pixel-sharp trace geometry stay coherent", () => {
  const dark = hexVariables(".sdsync-app");
  const exactPalette = new Map([
    ["sdsync-bg", "#0b0706"],
    ["sdsync-panel", "#160b08"],
    ["sdsync-border", "#4a1b10"],
    ["sdsync-trace", "#6b2718"],
    ["sdsync-fire", "#ff6a1a"],
    ["sdsync-fire-strong", "#d72e16"],
    ["sdsync-amber", "#ffd2a3"]
  ]);
  for (const [name, expected] of exactPalette) {
    assert.equal(dark.get(name), expected, `hellfire token --${name} drifted`);
  }
  assert.match(declarations(".sdsync-app"), /--sdsync-check-mark:\s*url\("data:image\/svg\+xml/,
    "checked semantic toggles need a package-owned local mark");
  assert.match(css, /background-image:\s*repeating-linear-gradient\(/);
  assert.equal((css.match(/(?:repeating-)?linear-gradient\(/g) || []).length, 1);
  assert.match(css, /border-left:\s*2px solid var\(--sdsync-trace\)/);
  assert.match(css, /--sdsync-shadow:\s*8px 8px 0 rgba\(0, 0, 0, 0\.42\)/);
  assert.doesNotMatch(css, /border-radius:\s*(?:[3-9]|[1-9][0-9]+)px|border-radius:\s*(?:50|999)%?/);
  assert.doesNotMatch(css, /--sdsync-green|#77f0bd|#32d396|41, 200, 139|103, 230, 176/i);
});

test("Refresh uses a compact rectangular border control", () => {
  assert.match(
    app,
    /<v-button\s+type="border"[\s\S]{0,280}>[\s\S]*?<template #icon><action-icon\s+:class="\{ 'sdsync-is-spinning': snapshotLoading \}"\s+name="refresh"\s*\/><\/template>Refresh<\/v-button>/
  );
  const topbarTheme = declarations(".sdsync-app .sdsync-topbar-actions > [class*=\"button\"],\n.sdsync-app .sdsync-topbar-actions button,\n.sdsync-app .sdsync-topbar-actions [role=\"button\"]");
  assert.match(topbarTheme, /min-width:\s*76px\s*!important/);
  assert.match(topbarTheme, /min-height:\s*30px\s*!important/);
  assert.match(topbarTheme, /border-radius:\s*2px\s*!important/);
});
