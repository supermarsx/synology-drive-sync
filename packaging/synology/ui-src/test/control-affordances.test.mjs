import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const css = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");
const app = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const security = await readFile(new URL("../src/SecurityPanel.vue", import.meta.url), "utf8");
const actionIcons = await readFile(new URL("../src/ActionIcon.js", import.meta.url), "utf8");

const rules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((match) => ({
  selectors: match[1].replace(/\/\*[\s\S]*?\*\//g, "").trim(),
  declarations: match[2]
}));

function ruleContaining(...selectors) {
  const rule = rules.find((candidate) => selectors.every((selector) => candidate.selectors.includes(selector)));
  assert.ok(rule, `missing a CSS rule containing ${selectors.join(", ")}`);
  return rule.declarations;
}

function ruleContainingDeclaration(selectors, declaration) {
  const rule = rules.find((candidate) =>
    selectors.every((selector) => candidate.selectors.includes(selector))
      && declaration.test(candidate.declarations)
  );
  assert.ok(rule, `missing ${declaration} for ${selectors.join(", ")}`);
  return rule.declarations;
}

function requireDeclaration(block, expression, message) {
  assert.match(block, expression, message);
}

function requireSingleControlFocus(block, label) {
  requireDeclaration(block, /outline:\s*(?:none|0)\s*!important/, `${label} must suppress the browser/DSM outer outline`);
  const focusBorder = /border(?:-color)?:\s*(?:[0-9]+px\s+solid\s+)?var\(--sdsync-focus\)\s*!important/.test(block);
  const insetFocus = /box-shadow:\s*inset\s+[^;]*var\(--sdsync-focus\)[^;]*!important/.test(block);
  assert.notEqual(
    focusBorder,
    insetFocus,
    `${label} must use exactly one authored focus border or inset ring`
  );
  if (focusBorder) requireDeclaration(block, /box-shadow:\s*none\s*!important/,
    `${label} focus border must not be combined with another ring`);
  if (insetFocus) assert.doesNotMatch(block, /box-shadow:\s*(?:0|[1-9])[^;]*var\(--sdsync-focus\)/,
    `${label} inset ring must not be combined with an external halo`);
}

function actionableButtons(source) {
  return [...source.matchAll(/<v-button\b[\s\S]*?<\/v-button>/g)].map((match) => match[0]);
}

function buttonLabel(button) {
  return button
    .replace(/<[^>]+>/g, " ")
    .replace(/\{\{[\s\S]*?\}\}/g, " dynamic action ")
    .replace(/\s+/g, " ")
    .trim();
}

function iconIdentity(button) {
  const actionIcon = button.match(/<action-icon\b[^>]*\bname="([a-z0-9-]+)"[^>]*\/?\s*>/i);
  if (actionIcon) return actionIcon[1].toLowerCase();
  const componentIcon = button.match(/\b(?:icon|prefix-icon|suffix-icon)="([a-z0-9-]+)"/i);
  if (componentIcon) return componentIcon[1].toLowerCase();
  const inlineIcon = button.match(
    /<span\b(?=[^>]*\bclass="[^"]*sdsync-action-icon[^"]*")(?=[^>]*\baria-hidden="true")(?=[^>]*\bdata-icon="([a-z0-9-]+)")[^>]*>[\s\S]*?<\/span>/i
  );
  return inlineIcon ? inlineIcon[1].toLowerCase() : "";
}

test("native controls suppress browser chrome and restore one authored focus ring", () => {
  const base = ruleContainingDeclaration(
    [
      '.sdsync-app input:not([type="checkbox"]):not([type="radio"])',
      ".sdsync-app textarea",
      ".sdsync-app select",
      '.sdsync-app [role="combobox"]'
    ],
    /outline:\s*(?:none|0)\s*!important/
  );
  requireDeclaration(base, /outline:\s*(?:none|0)\s*!important/, "base controls must suppress the browser outline");
  requireDeclaration(base, /box-shadow:\s*none\s*!important/, "base controls must suppress DSM/browser shadow rings");

  const wrappedInner = ruleContainingDeclaration(
    [
      '.sdsync-app [role="combobox"] input',
      '.sdsync-app [aria-haspopup="listbox"] input'
    ],
    /border:\s*0\s*!important/
  );
  requireDeclaration(wrappedInner, /outline:\s*(?:none|0)\s*!important/);
  requireDeclaration(wrappedInner, /background:\s*transparent\s*!important/);
  requireDeclaration(wrappedInner, /box-shadow:\s*none\s*!important/);

  const nativeFocus = ruleContainingDeclaration(
    [
      '.sdsync-app input:not([type="checkbox"]):not([type="radio"]):focus-visible',
      ".sdsync-app textarea:focus-visible",
      ".sdsync-app select:focus-visible"
    ],
    /outline:/
  );
  requireSingleControlFocus(nativeFocus, "native control focus");

  const dsmFocusOwner = ruleContainingDeclaration(
    [
      '.sdsync-app [role="combobox"]:focus-within',
      '.sdsync-app [aria-haspopup="listbox"]:focus-within'
    ],
    /outline:/
  );
  requireSingleControlFocus(dsmFocusOwner, "DSM control owner focus");

  const outlines = [...css.matchAll(/\boutline:\s*([^;]+);/g)].map((match) => match[1].trim());
  assert.ok(outlines.length > 0, "the AppWindow no longer defines focus outlines");
  for (const outline of outlines) {
    assert.match(
      outline,
      /^(?:(?:none|0)(?:\s*!important)?|2px solid var\(--sdsync-focus\)(?:\s*!important)?)$/,
      `unowned or grey/native-looking outline introduced: ${outline}`
    );
  }
});

test("input, select, and textarea states remain explicitly theme-owned", () => {
  const base = ruleContainingDeclaration(
    [
      '.sdsync-app input:not([type="checkbox"]):not([type="radio"])',
      ".sdsync-app textarea",
      ".sdsync-app select"
    ],
    /color:\s*var\(--sdsync-text\)\s*!important/
  );
  for (const declaration of [
    /color:\s*var\(--sdsync-text\)\s*!important/,
    /border(?:-color)?:\s*(?:1px\s+solid\s+)?var\(--sdsync-control-border\)\s*!important/,
    /background-color:\s*var\(--sdsync-control\)\s*!important/,
    /-webkit-text-fill-color:\s*var\(--sdsync-text\)\s*!important/
  ]) requireDeclaration(base, declaration, `base control state lacks ${declaration}`);

  const hover = ruleContaining(
    '.sdsync-app input:not([type="checkbox"]):not([type="radio"]):hover:not(:disabled):not([readonly])',
    ".sdsync-app textarea:hover:not(:disabled):not([readonly])",
    ".sdsync-app select:hover:not(:disabled)"
  );
  requireDeclaration(hover, /background-color:\s*var\(--sdsync-control-hover\)\s*!important/);
  requireDeclaration(hover, /border-color:\s*var\(--sdsync-focus\)\s*!important/);

  const disabled = ruleContaining(
    ".sdsync-app input:disabled",
    ".sdsync-app textarea:disabled",
    ".sdsync-app select:disabled"
  );
  for (const declaration of [
    /color:\s*var\(--sdsync-disabled\)\s*!important/,
    /background-color:\s*var\(--sdsync-control-disabled\)\s*!important/,
    /-webkit-text-fill-color:\s*var\(--sdsync-disabled\)\s*!important/,
    /opacity:\s*1\s*!important/
  ]) requireDeclaration(disabled, declaration, `disabled control state lacks ${declaration}`);

  const readonly = ruleContaining(
    ".sdsync-app input[readonly]",
    ".sdsync-app textarea[readonly]"
  );
  requireDeclaration(readonly, /color:\s*var\(--sdsync-muted\)\s*!important/);
  requireDeclaration(readonly, /background-color:\s*var\(--sdsync-panel-strong\)\s*!important/);
  requireDeclaration(readonly, /-webkit-text-fill-color:\s*var\(--sdsync-muted\)\s*!important/);

  const placeholders = ruleContaining(
    ".sdsync-app input::placeholder",
    ".sdsync-app textarea::placeholder",
    '.sdsync-app [role="combobox"]::placeholder'
  );
  requireDeclaration(placeholders, /color:\s*var\(--sdsync-placeholder\)\s*!important/);
  requireDeclaration(placeholders, /opacity:\s*1\s*!important/);

  const options = ruleContaining(".sdsync-app select option", ".sdsync-app select optgroup");
  requireDeclaration(options, /color:\s*var\(--sdsync-text\)/);
  requireDeclaration(options, /background:\s*var\(--sdsync-control\)/);

  const autofill = ruleContaining(
    ".sdsync-app input:-webkit-autofill",
    ".sdsync-app input:-webkit-autofill:hover",
    ".sdsync-app input:-webkit-autofill:focus"
  );
  requireDeclaration(autofill, /-webkit-box-shadow:\s*0 0 0 1000px var\(--sdsync-control\) inset\s*!important/);
});

test("right-hand semantic toggles own every visible hellfire state", () => {
  const base = ruleContaining(".sdsync-app .sdsync-toggle-row .sdsync-checkbox-input");
  for (const declaration of [
    /width:\s*22px\s*!important/,
    /height:\s*22px\s*!important/,
    /border:\s*1px solid var\(--sdsync-control-border\)\s*!important/,
    /appearance:\s*none\s*!important/,
    /background-color:\s*var\(--sdsync-control\)\s*!important/,
    /pointer-events:\s*auto\s*!important/
  ]) requireDeclaration(base, declaration, `semantic toggle base lacks ${declaration}`);

  const checked = ruleContaining(".sdsync-app .sdsync-toggle-row .sdsync-checkbox-input:checked");
  requireDeclaration(checked, /border-color:\s*var\(--sdsync-primary\)\s*!important/);
  requireDeclaration(checked, /background-color:\s*var\(--sdsync-primary\)\s*!important/);
  requireDeclaration(checked, /background-image:\s*var\(--sdsync-check-mark\)\s*!important/);

  const danger = ruleContaining(".sdsync-app .sdsync-toggle-row.is-danger .sdsync-checkbox-input:checked");
  requireDeclaration(danger, /background-color:\s*var\(--sdsync-danger\)\s*!important/);

  const disabled = ruleContaining(".sdsync-app .sdsync-toggle-row .sdsync-checkbox-input:disabled");
  requireDeclaration(disabled, /background-color:\s*var\(--sdsync-control-disabled\)\s*!important/);
  requireDeclaration(disabled, /cursor:\s*not-allowed\s*!important/);
});

test("keyboard focus remains visible on every actionable control family", () => {
  const focusContracts = [
    ['.sdsync-app button:not(.sdsync-nav-item):not(.sdsync-profile-row):not(.sdsync-routine-row):focus-visible'],
    [".sdsync-nav-item:focus-visible"],
    [".sdsync-profile-row:focus-visible", ".sdsync-routine-row:focus-visible"],
    [".sdsync-app .sdsync-toggle-row .sdsync-checkbox-input:focus-visible"],
    [".sdsync-field-tip-trigger:focus-visible"],
    [".sdsync-advanced summary:focus-visible"],
    [".sdsync-weekdays input:focus-visible + span"],
    [".sdsync-app pre:focus-visible"],
    [".sdsync-modal:focus-visible"],
    ['.sdsync-select-dropdown [role="option"]:focus-visible']
  ];
  for (const selectors of focusContracts) {
    const block = ruleContainingDeclaration(
      selectors,
      /outline:\s*2px solid var\(--sdsync-focus\)(?:\s*!important)?/
    );
    requireDeclaration(block, /outline:\s*2px solid var\(--sdsync-focus\)(?:\s*!important)?/,
      `${selectors.join(", ")} lacks a visible package focus outline`);
  }
});

test("native single selects use one stable package-owned arrow", () => {
  const appearance = ruleContainingDeclaration([".sdsync-app select"], /(?<!-webkit-)appearance:\s*none/);
  requireDeclaration(appearance, /(?<!-webkit-)appearance:\s*none(?:\s*!important)?/,
    "the browser-native select arrow must be disabled");

  const arrow = ruleContainingDeclaration([".sdsync-app select"], /background-image:/);
  requireDeclaration(
    arrow,
    /background-image:\s*(?:var\(--sdsync-select-arrow\)|url\(["']?data:image\/svg\+xml[^)]*\))(?:\s*!important)?/i,
    "single selects need a local deterministic arrow asset"
  );
  requireDeclaration(arrow, /background-repeat:\s*no-repeat(?:\s*!important)?/);
  requireDeclaration(arrow, /background-position:\s*right\s+(?:[0-9.]+(?:px|rem|em))\s+center(?:\s*!important)?/);
  requireDeclaration(arrow, /background-size:\s*[0-9.]+(?:px|rem|em)(?:\s+[0-9.]+(?:px|rem|em))?(?:\s*!important)?/);
  requireDeclaration(arrow, /padding:\s*[^;]*\s[0-9.]+(?:px|rem|em)\s[^;]*!important|padding-right:\s*[0-9.]+(?:px|rem|em)\s*!important/,
    "select text needs room for the package arrow");
  assert.doesNotMatch(arrow, /url\(["']?https?:/i, "select arrow must not depend on a remote asset");

  const multiple = ruleContainingDeclaration([".sdsync-app select[multiple]"], /background-image:\s*none/);
  requireDeclaration(multiple, /background-image:\s*none\s*!important/,
    "multi-selects must not inherit the single-select arrow");
});

test("action buttons retain visible labels and meaningful icon coverage", () => {
  const buttons = [...actionableButtons(app), ...actionableButtons(security)];
  assert.ok(buttons.length >= 20, "fixture no longer covers the AppWindow action surface");
  assert.match(actionIcons, /class:\s*"sdsync-action-icon"/, "icons need a stable package-owned class");
  assert.match(actionIcons, /"aria-hidden":\s*"true"/, "decorative icons must stay out of the accessibility tree");
  assert.match(actionIcons, /focusable:\s*"false"/, "decorative SVGs must not become extra tab stops");

  const labelled = buttons.filter((button) => buttonLabel(button) || /\baria-label=/.test(button));
  assert.equal(labelled.length, buttons.length, "an icon must never replace an action's accessible name");

  const iconButtons = buttons.filter((button) => iconIdentity(button));
  assert.ok(
    iconButtons.length / buttons.length >= 0.8,
    `only ${iconButtons.length}/${buttons.length} action buttons expose package-owned icon semantics`
  );
  const identities = new Set(iconButtons.map(iconIdentity));
  assert.ok(identities.size >= 8, `only ${identities.size} distinct action icons remain`);

  for (const label of [
    "Help",
    "Refresh",
    "Retry",
    "Plan all profiles",
    "Run all profiles",
    "New profile",
    "Save now",
    "Run doctor",
    "Clear view"
  ]) {
    const button = buttons.find((candidate) => (
      buttonLabel(candidate).includes(label)
      || (label === "Save now" && candidate.includes("'Save now'"))
    ));
    assert.ok(button, `missing critical action ${label}`);
    assert.ok(iconIdentity(button), `critical action ${label} lacks a meaningful package icon`);
  }
});
