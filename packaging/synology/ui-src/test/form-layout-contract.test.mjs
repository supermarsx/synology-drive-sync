import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const app = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const security = await readFile(new URL("../src/SecurityPanel.vue", import.meta.url), "utf8");
const css = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");
const controlLayout = await readFile(new URL("../src/controlLayout.js", import.meta.url), "utf8");
const webpack = await readFile(new URL("../webpack.config.js", import.meta.url), "utf8");

function openingTags(source, component) {
  return source.match(new RegExp(`<${component}\\b[^>]*>`, "g")) || [];
}

function ownedCheckboxRows(source, helpComponent) {
  const expression = new RegExp(
    `<div\\b(?=[^>]*\\bclass="[^"]*\\bsdsync-check-row\\b[^"]*")[^>]*>`
      + `\\s*<v-checkbox\\b[\\s\\S]*?<\\/v-checkbox>\\s*`
      + `<${helpComponent}\\b[^>]*\\/>\\s*<\\/div>`,
    "g"
  );
  return source.match(expression) || [];
}

function rules() {
  return [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((match) => ({
    selectors: match[1].trim(),
    declarations: match[2]
  }));
}

function declarationsForRuleContaining(...selectors) {
  const rule = rules().find((candidate) => selectors.every((selector) => candidate.selectors.includes(selector)));
  assert.ok(rule, `missing owned CSS rule for ${selectors.join(", ")}`);
  return rule.declarations;
}

function declarationsForRuleContainingDeclaration(declaration, ...selectors) {
  const rule = rules().find((candidate) =>
    selectors.every((selector) => candidate.selectors.includes(selector))
      && declaration.test(candidate.declarations)
  );
  assert.ok(rule, `missing owned CSS declaration for ${selectors.join(", ")}`);
  return rule.declarations;
}

function declarationsForExactSelector(selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = css.match(new RegExp(`(?:^|\\n)${escaped}\\s*\\{([^{}]*)\\}`, "m"));
  assert.ok(match, `missing exact CSS rule ${selector}`);
  return match[1];
}

test("every DSM field component exposes a package-owned rendered-root hook", () => {
  const contracts = [
    ["v-form-item", "sdsync-form-item"],
    ["v-checkbox", "sdsync-checkbox-control"],
    ["v-single-select", "sdsync-select-control"],
    ["v-input", "sdsync-input-control"]
  ];
  for (const source of [app, security]) {
    for (const [component, hook] of contracts) {
      const tags = openingTags(source, component);
      assert.ok(tags.length > 0, `${component} is absent from a DSM view`);
      for (const tag of tags) {
        assert.match(tag, new RegExp(`\\bclass="[^"]*\\b${hook}\\b`), `${component} lacks ${hook}: ${tag}`);
        assert.equal((tag.match(/\bclass=/g) || []).length, 1, `${component} has duplicate class attributes: ${tag}`);
      }
    }
  }
});

test("every DSM checkbox keeps its label and tooltip in one owned bounded row", () => {
  const appRows = ownedCheckboxRows(app, "control-help");
  const securityRows = ownedCheckboxRows(security, "policy-help");
  assert.equal(appRows.length, openingTags(app, "v-checkbox").length, "App checkbox/help siblings escaped their row");
  assert.equal(securityRows.length, openingTags(security, "v-checkbox").length, "Security checkbox/help siblings escaped their row");

  const row = declarationsForExactSelector(".sdsync-check-row");
  assert.match(row, /grid-template-columns:\s*minmax\(0, 1fr\) 20px/);
  assert.match(row, /width:\s*100%/);
  assert.match(row, /overflow:\s*visible/);

  const checkbox = declarationsForRuleContainingDeclaration(
    /grid-row:\s*1/,
    ".sdsync-app .sdsync-check-row > .sdsync-checkbox-control"
  );
  assert.match(checkbox, /width:\s*100%\s*!important/);
  assert.match(checkbox, /grid-column:\s*1/);
  assert.match(checkbox, /margin:\s*0\s*!important/);

  const privateShells = declarationsForExactSelector(".sdsync-app .sdsync-control-shell");
  assert.match(privateShells, /max-width:\s*100%\s*!important/);
  assert.match(privateShells, /min-width:\s*0\s*!important/);
  assert.match(privateShells, /margin:\s*0\s*!important/);

  const focus = declarationsForExactSelector(".sdsync-app .sdsync-checkbox-control:focus-within");
  assert.match(focus, /outline:\s*2px solid var\(--sdsync-focus\)\s*!important/);
  assert.doesNotMatch(css, /\.v-checkbox/, "CSS must not guess that a Vue registration name survives rendering");
  assert.doesNotMatch(
    css,
    /\.sdsync-app \.sdsync-checkbox-control input\[type="checkbox"\]\s*\{/,
    "DSM retains ownership of its checkbox input geometry and position"
  );
  assert.doesNotMatch(css, /\.sdsync-checkbox-control[^,{]*\[class\*="(?:icon|box)"\]/,
    "the SDK retains ownership of its private tick glyph");
});

test("form roots reset only their structural children and exact owned controls", () => {
  const rows = declarationsForRuleContaining(
    ".sdsync-app .sdsync-form-grid > .sdsync-form-item",
    ".sdsync-app .sdsync-log-policy-grid > .sdsync-form-item",
    ".sdsync-app form.sdsync-panel:not(.sdsync-settings-panel) > .sdsync-form-item",
    ".sdsync-app .sdsync-danger-fieldset > .sdsync-form-item"
  );
  assert.match(rows, /grid-template-columns:\s*minmax\(0, 1fr\)\s*!important/);
  assert.match(rows, /gap:\s*5px\s*!important/);
  assert.match(rows, /margin:\s*0\s*!important/);

  const structuralChildren = declarationsForExactSelector(".sdsync-app .sdsync-form-item > *");
  assert.match(structuralChildren, /width:\s*100%\s*!important/);
  assert.match(structuralChildren, /margin:\s*0\s*!important/);
  assert.match(structuralChildren, /padding:\s*0\s*!important/);
  assert.match(structuralChildren, /box-sizing:\s*border-box/);

  const controls = declarationsForRuleContaining(
    ".sdsync-app .sdsync-form-item .sdsync-input-control",
    ".sdsync-app .sdsync-form-item .sdsync-select-control",
    ".sdsync-app .sdsync-form-item .sdsync-native-input"
  );
  assert.match(controls, /width:\s*100%\s*!important/);
  assert.match(controls, /min-width:\s*0/);
  assert.match(controls, /box-sizing:\s*border-box/);

  assert.doesNotMatch(css, /\.v-form-item/, "CSS must not rely on unrendered Vue tag names");
  assert.ok(!css.includes('.sdsync-app [class*="input"]'), "input styling leaked back to DSM private classes");
  assert.ok(!css.includes('.sdsync-app [class*="select"]'), "select styling leaked back to DSM private classes");
  assert.doesNotMatch(css, /\.sdsync-form-item\s+\*/,
    "form layout must not rewrite arbitrary nested component descendants");
});

test("input and select internals remain one row through arbitrary private DSM shells", () => {
  const select = declarationsForRuleContainingDeclaration(
    /display:\s*inline-flex\s*!important/,
    ".sdsync-app .sdsync-select-control",
    '.sdsync-app .sdsync-select-control [role="combobox"]',
    '.sdsync-app .sdsync-select-control [aria-haspopup="listbox"]'
  );
  assert.match(select, /flex-flow:\s*row nowrap\s*!important/);
  assert.match(select, /align-items:\s*center\s*!important/);

  const shells = declarationsForRuleContainingDeclaration(
    /flex:\s*1 1 auto\s*!important/,
    ".sdsync-app .sdsync-control-shell"
  );
  assert.match(shells, /width:\s*100%\s*!important/);
  assert.match(shells, /max-width:\s*100%\s*!important/);
  assert.match(shells, /min-width:\s*0\s*!important/);
  assert.match(shells, /margin:\s*0\s*!important/);

  const inputControl = declarationsForRuleContaining(
    '.sdsync-app .sdsync-input-control input:not([type="checkbox"]):not([type="radio"])',
    ".sdsync-app .sdsync-input-control textarea"
  );
  assert.match(inputControl, /width:\s*0\s*!important/);
  assert.match(inputControl, /flex:\s*1 1 auto\s*!important/);

  const input = declarationsForRuleContaining(
    '.sdsync-app [role="combobox"] input',
    '.sdsync-app [aria-haspopup="listbox"] input'
  );
  assert.match(input, /width:\s*0\s*!important/);
  assert.match(input, /flex:\s*1 1 auto\s*!important/);
});

test("Settings aligns label and control then stacks at short width", () => {
  const wide = declarationsForRuleContaining(
    ".sdsync-settings-panel > .sdsync-form-item",
    ".sdsync-horizontal-form > .sdsync-form-item"
  );
  assert.match(wide, /grid-template-columns:\s*minmax\(150px, 220px\) minmax\(0, 1fr\)\s*!important/);
  assert.match(wide, /align-items:\s*center\s*!important/);
  assert.match(wide, /gap:\s*16px\s*!important/);

  const controlColumn = declarationsForRuleContainingDeclaration(
    /grid-column:\s*2\s*!important/,
    ".sdsync-settings-panel > .sdsync-form-item > .sdsync-form-control-cell",
    ".sdsync-horizontal-form > .sdsync-form-item > .sdsync-form-control-cell"
  );
  assert.match(controlColumn, /grid-row:\s*1\s*!important/);
  assert.doesNotMatch(css, /\.sdsync-form-item\s*>\s*:first-child/,
    "control placement must not depend on undocumented DSM child order");

  const shortViewport = css.slice(css.indexOf("@media (max-width: 720px)"));
  assert.match(
    shortViewport,
    /\.sdsync-settings-panel > \.sdsync-form-item,[\s\S]*?grid-template-columns:\s*minmax\(0, 1fr\)\s*!important/
  );
  assert.match(shortViewport, /\.sdsync-form-grid[\s\S]*?grid-template-columns:\s*1fr/);

  const shortOwnedForm = css.slice(css.indexOf(".sdsync-settings-panel.sdsync-compact-form"));
  assert.ok(shortOwnedForm.length < css.length, "missing Chrome 88-compatible AppWindow width fallback");
  assert.match(
    shortOwnedForm,
    /\.sdsync-settings-panel\.sdsync-compact-form > \.sdsync-form-item,[\s\S]*?grid-template-columns:\s*minmax\(0, 1fr\)\s*!important/
  );
  assert.match(shortOwnedForm, /grid-column:\s*1\s*!important[\s\S]*?grid-row:\s*2\s*!important/);
});

test("critical form layout has an explicit Chrome 88 compatibility path", () => {
  assert.match(webpack, /targets:\s*\{\s*chrome:\s*["']88["']\s*\}/);
  assert.doesNotMatch(css, /:has\(/, "critical layout must not require relational selectors unavailable in Chrome 88");

  const progressiveStart = css.indexOf("@container (max-width: 720px)");
  assert.ok(progressiveStart > 0, "container-query enhancement is missing");
  const baselineCss = css.slice(0, progressiveStart);
  assert.match(baselineCss, /\.sdsync-app \.sdsync-control-shell\s*\{/);
  assert.match(baselineCss, /\.sdsync-settings-panel\.sdsync-compact-form > \.sdsync-form-item/);
  assert.match(baselineCss, /> \.sdsync-form-control-cell/);

  assert.match(controlLayout, /classList\.add\("sdsync-control-shell", typeClass\)/);
  assert.match(controlLayout, /classList\.add\("sdsync-form-control-cell"\)/);
  assert.match(controlLayout, /classList\.toggle\("sdsync-compact-form"/);
  assert.match(controlLayout, /new ResizeObserver\(/);
  assert.match(app, /installControlLayout\(this\.\$el\)/);
  assert.match(app, /this\.controlLayoutCleanup\(\)/);
});
