import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const app = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const security = await readFile(new URL("../src/SecurityPanel.vue", import.meta.url), "utf8");
const css = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");

function checkboxCount(source) {
  return (source.match(/<v-checkbox\b/g) || []).length;
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

function declarationsForRuleContaining(...selectors) {
  const rules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((match) => ({
    selectors: match[1].trim(),
    declarations: match[2]
  }));
  const rule = rules.find((candidate) => selectors.every((selector) => candidate.selectors.includes(selector)));
  assert.ok(rule, `missing owned CSS rule for ${selectors.join(", ")}`);
  return rule.declarations;
}

function declarationsForRuleContainingDeclaration(declaration, ...selectors) {
  const rules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((match) => ({
    selectors: match[1].trim(),
    declarations: match[2]
  }));
  const rule = rules.find((candidate) =>
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

test("every DSM checkbox keeps its label and tooltip in one owned bounded row", () => {
  const appRows = ownedCheckboxRows(app, "control-help");
  const securityRows = ownedCheckboxRows(security, "policy-help");
  assert.equal(appRows.length, checkboxCount(app), "App checkbox/help siblings escaped their row wrapper");
  assert.equal(securityRows.length, checkboxCount(security), "Security checkbox/help siblings escaped their row wrapper");

  for (const model of ["profileForm", "routineForm", "doctorForm", "alertForm", "notificationForm"]) {
    assert.ok(appRows.some((row) => row.includes(`v-model="${model}.`)), `${model} lacks an owned checkbox row`);
  }
  assert.equal((security.match(/class="sdsync-check-row sdsync-policy-control"/g) || []).length, 2);

  const row = declarationsForExactSelector(".sdsync-check-row");
  assert.match(row, /grid-template-columns:\s*minmax\(0, 1fr\) 20px/);
  assert.match(row, /width:\s*100%/);
  assert.match(row, /margin:\s*0/);
  assert.match(row, /overflow:\s*visible/);

  const checkbox = declarationsForRuleContainingDeclaration(
    /grid-row:\s*1/,
    ".sdsync-app .sdsync-check-row > .v-checkbox",
    ".sdsync-app .sdsync-check-row > label.v-checkbox"
  );
  assert.match(checkbox, /width:\s*100%\s*!important/);
  assert.match(checkbox, /grid-row:\s*1/);
  assert.match(checkbox, /min-width:\s*0/);
  assert.match(checkbox, /margin:\s*0\s*!important/);
  assert.match(checkbox, /padding:\s*0\s*!important/);

  const help = declarationsForExactSelector(".sdsync-check-row > .sdsync-field-tip");
  assert.match(help, /grid-column:\s*2/);
  assert.match(help, /grid-row:\s*1/);

  const focus = declarationsForRuleContaining(
    ".sdsync-app .sdsync-check-row > .v-checkbox:focus-within",
    ".sdsync-app .sdsync-check-row > label.v-checkbox:focus-within"
  );
  assert.match(focus, /outline:\s*2px solid var\(--sdsync-focus\)\s*!important/);

  assert.doesNotMatch(css, /\.sdsync-check-row[^,{]*\[class\*="(?:icon|box)"\]/,
    "the SDK must retain ownership of checkbox tick positioning and geometry");
  assert.doesNotMatch(css, /\.sdsync-app\s+\[class\*="checkbox"\]/,
    "checkbox theming must use the exact rendered v-checkbox root below the owned row");
});

test("owned form roots remove SDK margins without descendant-wide rewrites", () => {
  const rows = declarationsForRuleContaining(
    ".sdsync-app .sdsync-form-grid > .v-form-item",
    ".sdsync-app .sdsync-log-policy-grid > .v-form-item",
    ".sdsync-app form.sdsync-panel:not(.sdsync-settings-panel) > .v-form-item",
    ".sdsync-app .sdsync-danger-fieldset > .v-form-item"
  );
  assert.match(rows, /grid-template-columns:\s*minmax\(0, 1fr\)\s*!important/);
  assert.match(rows, /gap:\s*5px\s*!important/);
  assert.match(rows, /margin:\s*0\s*!important/);
  assert.match(rows, /padding:\s*0\s*!important/);

  assert.doesNotMatch(css, /\.sdsync-app form \[class\*="label"\]/,
    "form labels must be normalized only as direct children of owned row roots");
  assert.doesNotMatch(css, /\.sdsync-settings-panel \[class\*="form-item"\]/,
    "Settings must not style arbitrary descendant form items");

  const inputs = declarationsForRuleContaining(
    ".sdsync-app .sdsync-form-grid > .v-form-item > .v-form-item-input",
    ".sdsync-app .sdsync-log-policy-grid > .v-form-item > .v-form-item-input",
    ".sdsync-app form.sdsync-panel:not(.sdsync-settings-panel) > .v-form-item > .v-form-item-input",
    ".sdsync-app .sdsync-danger-fieldset > .v-form-item > .v-form-item-input"
  );
  assert.match(inputs, /width:\s*100%\s*!important/);
  assert.match(inputs, /max-width:\s*100%/);
  assert.match(inputs, /margin:\s*0\s*!important/);
  assert.match(inputs, /padding:\s*0\s*!important/);
  assert.match(inputs, /box-sizing:\s*border-box/);

  const nativeInput = declarationsForRuleContaining(".sdsync-native-input");
  assert.match(nativeInput, /width:\s*100%/);
  assert.match(nativeInput, /max-width:\s*100%/);
  assert.match(nativeInput, /margin:\s*0\s*!important/);
  assert.match(nativeInput, /box-sizing:\s*border-box/);
});

test("Settings aligns label, adjacent help, and control then stacks at short width", () => {
  const wide = declarationsForRuleContaining(
    ".sdsync-settings-panel > .v-form-item",
    ".sdsync-horizontal-form > .v-form-item"
  );
  assert.match(wide, /grid-template-columns:\s*minmax\(150px, 220px\) minmax\(0, 1fr\)\s*!important/);
  assert.match(wide, /align-items:\s*center\s*!important/);
  assert.match(wide, /gap:\s*16px\s*!important/);
  assert.match(wide, /margin:\s*0\s*!important/);
  assert.doesNotMatch(wide, /grid-template-columns:\s*minmax\(0, 1fr\)\s*!important/,
    "the vertical row contract must not override horizontal Settings rows");

  const inputs = declarationsForRuleContaining(
    ".sdsync-settings-panel > .v-form-item > .v-form-item-input",
    ".sdsync-horizontal-form > .v-form-item > .v-form-item-input"
  );
  assert.match(inputs, /width:\s*100%\s*!important/);
  assert.match(inputs, /margin:\s*0\s*!important/);
  assert.match(inputs, /padding:\s*0\s*!important/);

  const shortViewport = css.slice(css.indexOf("@media (max-width: 720px)"));
  assert.match(
    shortViewport,
    /\.sdsync-settings-panel > \.v-form-item,[\s\S]*?grid-template-columns:\s*minmax\(0, 1fr\)\s*!important/
  );
  assert.match(shortViewport, /\.sdsync-form-grid[\s\S]*?grid-template-columns:\s*1fr/);
});
