import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const app = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const security = await readFile(new URL("../src/SecurityPanel.vue", import.meta.url), "utf8");
const css = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");
const controlLayout = await readFile(new URL("../src/controlLayout.js", import.meta.url), "utf8");
const webpack = await readFile(new URL("../webpack.config.js", import.meta.url), "utf8");
const physicalFixture = await readFile(
  new URL("./fixtures/dsm-physical-control-dom.html", import.meta.url),
  "utf8"
);

function openingTags(source, component) {
  return source.match(new RegExp(`<${component}\\b[^>]*>`, "g")) || [];
}

function ownedToggleRows(source, helpComponent) {
  const expression = new RegExp(
    `<div\\b(?=[^>]*\\bclass="[^"]*\\bsdsync-toggle-row\\b[^"]*")[^>]*>`
      + `[\\s\\S]*?<span\\b(?=[^>]*\\bclass="[^"]*\\bsdsync-toggle-label\\b[^"]*")[^>]*>`
      + `[\\s\\S]*?<${helpComponent}\\b[^>]*\\/>[\\s\\S]*?<\\/span>\\s*`
      + `<v-checkbox\\b[\\s\\S]*?(?:\\/>|<\\/v-checkbox>)\\s*<\\/div>`,
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

test("every DSM checkbox uses one right-hand package-owned semantic toggle row", () => {
  const appRows = ownedToggleRows(app, "control-help");
  const securityRows = ownedToggleRows(security, "policy-help");
  assert.equal(appRows.length, openingTags(app, "v-checkbox").length, "App checkbox escaped its labelled toggle row");
  assert.equal(securityRows.length, openingTags(security, "v-checkbox").length, "Security checkbox escaped its labelled toggle row");

  const row = declarationsForExactSelector(".sdsync-toggle-row");
  assert.match(row, /grid-template-columns:\s*minmax\(145px, 190px\) minmax\(44px, 1fr\)/);
  assert.match(row, /width:\s*100%/);
  assert.match(row, /overflow:\s*visible/);

  const checkbox = declarationsForRuleContainingDeclaration(
    /grid-row:\s*1/,
    ".sdsync-app .sdsync-toggle-row > .sdsync-checkbox-control"
  );
  assert.match(checkbox, /grid-column:\s*2/);
  assert.match(checkbox, /justify-self:\s*end/);
  assert.match(checkbox, /max-width:\s*22px\s*!important/);
  assert.match(checkbox, /margin:\s*0\s*!important/);

  const privateShells = declarationsForExactSelector(".sdsync-app .sdsync-control-shell");
  assert.match(privateShells, /max-width:\s*100%\s*!important/);
  assert.match(privateShells, /min-width:\s*0\s*!important/);
  assert.match(privateShells, /margin:\s*0\s*!important/);
  assert.match(privateShells, /border:\s*0\s*!important/);
  assert.match(privateShells, /background-color:\s*transparent\s*!important/);
  assert.match(privateShells, /background-image:\s*none\s*!important/);

  const focus = declarationsForExactSelector(".sdsync-app .sdsync-checkbox-control:focus-within");
  assert.match(focus, /outline:\s*0\s*!important/);
  assert.doesNotMatch(css, /\.v-checkbox/, "CSS must not guess that a Vue registration name survives rendering");
  assert.doesNotMatch(css, /\.sdsync-checkbox-control[^,{]*\[class\*="(?:icon|box)"\]/,
    "the SDK retains ownership of its private tick glyph");

  const hiddenSdkParts = declarationsForRuleContaining(
    ".sdsync-app .sdsync-toggle-row .sdsync-checkbox-label",
    ".sdsync-app .sdsync-toggle-row .sdsync-checkbox-glyph"
  );
  assert.match(hiddenSdkParts, /display:\s*none\s*!important/);

  const semanticToggle = declarationsForExactSelector(
    ".sdsync-app .sdsync-toggle-row .sdsync-checkbox-input"
  );
  assert.match(semanticToggle, /position:\s*relative\s*!important/);
  assert.match(semanticToggle, /width:\s*22px\s*!important/);
  assert.match(semanticToggle, /opacity:\s*1\s*!important/);
  assert.match(semanticToggle, /pointer-events:\s*auto\s*!important/);
  assert.match(semanticToggle, /appearance:\s*none\s*!important/);
  assert.match(semanticToggle, /background-color:\s*var\(--sdsync-control\)\s*!important/);
});

test("sanitized fixture preserves the captured DSM control hierarchy", () => {
  assert.match(
    physicalFixture,
    /class="v-form-item-input fit-container"[\s\S]*?class="v-form-item-control fit-container"[\s\S]*?class="v-textfield sdsync-input-control fit-container"[\s\S]*?class="v-textfield-input fit-container"[\s\S]*?<input[^>]*class="v-textfield-input-element fit-container"/,
    "fixture must preserve the fit-container textfield and nested form-item shells"
  );
  assert.match(
    physicalFixture,
    /class="v-textfield sdsync-input-control fit-container"[\s\S]*?class="v-textfield-input fit-container"[\s\S]*?<textarea[^>]*class="v-textfield-input-element fit-container"/,
    "fixture must preserve the textarea variant of the same SDK wrapper"
  );
  assert.match(physicalFixture, /class="sdsync-toggle-row"[\s\S]*?class="sdsync-toggle-label"[\s\S]*?class="dsm-checkbox sdsync-checkbox-control v-checkbox-wrapper"[\s\S]*?<i\b[^>]*v-checkbox-icon[\s\S]*?<input\b[^>]*v-checkbox-input[\s\S]*?<label\b[^>]*v-checkbox-label/);
  assert.match(physicalFixture, /id="checkbox"[^>]*\bchecked\b/, "fixture must exercise the visible checked state");
  assert.doesNotMatch(physicalFixture, /<label[^>]*>\s*<input\b/, "captured DSM checkbox input is a label sibling");
  assert.match(physicalFixture, /v-select2-wrapper[\s\S]*?class="input-wrapper[\s\S]*?class="v-select-ul-wrap[\s\S]*?<input[^>]*aria-haspopup="listbox"/);
  assert.doesNotMatch(physicalFixture, /(?:_SSID|SynoToken|quickconnect|https?:\/\/|Cookie:|session)/i,
    "fixture must contain structure only, never captured DSM session data");
});

test("form roots make exact owned controls full-width without private DSM leakage", () => {
  const rows = declarationsForRuleContaining(
    ".sdsync-app .sdsync-form-grid > .sdsync-form-item",
    ".sdsync-app .sdsync-log-policy-grid > .sdsync-form-item",
    ".sdsync-app form.sdsync-panel:not(.sdsync-settings-panel) > .sdsync-form-item",
    ".sdsync-app .sdsync-danger-fieldset > .sdsync-form-item"
  );
  assert.match(rows, /grid-template-columns:\s*minmax\(145px, 190px\) minmax\(260px, 1fr\)\s*!important/);
  assert.match(rows, /gap:\s*14px\s*!important/);
  assert.match(rows, /margin:\s*0\s*!important/);
  assert.match(rows, /padding:\s*8px 0\s*!important/);

  const structuralChildren = declarationsForExactSelector(".sdsync-app .sdsync-form-item > *");
  assert.match(structuralChildren, /width:\s*100%\s*!important/);
  assert.match(structuralChildren, /margin:\s*0\s*!important/);
  assert.match(structuralChildren, /padding:\s*0\s*!important/);
  assert.match(structuralChildren, /height:\s*auto\s*!important/);
  assert.match(structuralChildren, /box-sizing:\s*border-box/);

  const controls = declarationsForRuleContaining(
    ".sdsync-app .sdsync-form-item .sdsync-input-control",
    ".sdsync-app .sdsync-form-item .sdsync-select-control",
    ".sdsync-app .sdsync-form-item .sdsync-native-input"
  );
  assert.match(controls, /width:\s*100%\s*!important/);
  assert.match(controls, /min-width:\s*0/);
  assert.match(controls, /box-sizing:\s*border-box/);

  const controlPath = declarationsForRuleContaining(
    ".sdsync-app .sdsync-form-control-shell",
    ".sdsync-app .sdsync-form-control-cell"
  );
  assert.match(controlPath, /height:\s*auto\s*!important/);
  assert.match(controlPath, /min-height:\s*0\s*!important/);
  assert.match(controlPath, /margin:\s*0\s*!important/);
  assert.match(controlPath, /background-color:\s*transparent\s*!important/);
  assert.match(controlPath, /box-shadow:\s*none\s*!important/);

  const fillPath = declarationsForRuleContaining(
    ".sdsync-app .sdsync-form-item[class] .sdsync-form-control-shell",
    ".sdsync-app .sdsync-form-item[class] > .sdsync-form-control-cell"
  );
  assert.match(fillPath, /inline-size:\s*100%\s*!important/);
  assert.match(fillPath, /width:\s*100%\s*!important/);
  assert.match(fillPath, /max-inline-size:\s*100%\s*!important/);
  assert.match(fillPath, /min-inline-size:\s*0\s*!important/);
  assert.match(fillPath, /flex:\s*1 1 0%\s*!important/);
  assert.match(fillPath, /margin:\s*0\s*!important/);
  assert.match(fillPath, /background-color:\s*transparent\s*!important/);

  assert.doesNotMatch(css, /\.v-form-item/, "CSS must not rely on unrendered Vue tag names");
  assert.ok(!css.includes('.sdsync-app [class*="input"]'), "input styling leaked back to DSM private classes");
  assert.ok(!css.includes('.sdsync-app [class*="select"]'), "select styling leaked back to DSM private classes");
  assert.doesNotMatch(css, /\.sdsync-form-item\s+\*/,
    "form layout must not rewrite arbitrary nested component descendants");
});

test("input and select internals remain one row through arbitrary private DSM shells", () => {
  const select = declarationsForRuleContainingDeclaration(
    /display:\s*inline-flex\s*!important/,
    '.sdsync-app .sdsync-select-control[role="combobox"]',
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

  const markedOwner = declarationsForRuleContaining(
    ".sdsync-app .sdsync-control-owner.sdsync-input-control",
    ".sdsync-app .sdsync-control-owner.sdsync-select-control"
  );
  assert.match(markedOwner, /inline-size:\s*100%\s*!important/);
  assert.match(markedOwner, /width:\s*100%\s*!important/);
  assert.match(markedOwner, /max-inline-size:\s*100%\s*!important/);
  assert.match(markedOwner, /min-inline-size:\s*0\s*!important/);
  assert.match(markedOwner, /flex:\s*1 1 0%\s*!important/);

  const markedShell = declarationsForRuleContaining(
    ".sdsync-app .sdsync-control-owner.sdsync-input-control .sdsync-control-shell.sdsync-input-shell",
    ".sdsync-app .sdsync-control-owner.sdsync-select-control .sdsync-control-shell.sdsync-select-shell"
  );
  assert.match(markedShell, /inline-size:\s*100%\s*!important/);
  assert.match(markedShell, /width:\s*100%\s*!important/);
  assert.match(markedShell, /flex:\s*1 1 0%\s*!important/);
  assert.match(markedShell, /margin:\s*0\s*!important/);
  assert.match(markedShell, /padding:\s*0\s*!important/);
  assert.match(markedShell, /background-color:\s*transparent\s*!important/);

  const semanticFill = declarationsForRuleContaining(
    '.sdsync-app .sdsync-control-owner.sdsync-input-control input.sdsync-semantic-control:not([type="checkbox"]):not([type="radio"])',
    ".sdsync-app .sdsync-control-owner.sdsync-input-control textarea.sdsync-semantic-control"
  );
  assert.match(semanticFill, /inline-size:\s*100%\s*!important/);
  assert.match(semanticFill, /width:\s*100%\s*!important/);
  assert.match(semanticFill, /max-inline-size:\s*100%\s*!important/);
  assert.match(semanticFill, /max-width:\s*100%\s*!important/);
  assert.match(semanticFill, /min-inline-size:\s*0\s*!important/);
  assert.match(semanticFill, /min-width:\s*0\s*!important/);
  assert.match(semanticFill, /flex:\s*1 1 0%\s*!important/);
  assert.match(semanticFill, /margin:\s*0\s*!important/);
  assert.match(semanticFill, /background:\s*transparent\s*!important/);

  const inputControl = declarationsForRuleContaining(
    '.sdsync-app .sdsync-input-control input:not([type="checkbox"]):not([type="radio"])',
    ".sdsync-app .sdsync-input-control textarea"
  );
  assert.match(inputControl, /width:\s*100%\s*!important/);
  assert.match(inputControl, /max-width:\s*100%\s*!important/);
  assert.match(inputControl, /min-width:\s*0\s*!important/);
  assert.match(inputControl, /flex:\s*1 1 0%\s*!important/);

  const input = declarationsForRuleContaining(
    '.sdsync-app [role="combobox"] input',
    '.sdsync-app [aria-haspopup="listbox"] input'
  );
  assert.match(input, /width:\s*100%\s*!important/);
  assert.match(input, /max-width:\s*100%\s*!important/);
  assert.match(input, /min-width:\s*0\s*!important/);
  assert.match(input, /flex:\s*1 1 0%\s*!important/);

  const physicalSelect = declarationsForExactSelector(
    '.sdsync-app .sdsync-select-control:not(input):not(textarea):not(select):not([role="combobox"]):not([aria-haspopup="listbox"])'
  );
  assert.match(physicalSelect, /display:\s*block\s*!important/);
  assert.match(physicalSelect, /padding:\s*3px 29px 3px 11px\s*!important/);
  assert.match(physicalSelect, /border:\s*1px solid var\(--sdsync-control-border\)\s*!important/);

  const physicalInput = declarationsForExactSelector(
    '.sdsync-app .sdsync-select-control input:not([type="checkbox"]):not([type="radio"])'
  );
  assert.match(physicalInput, /border:\s*0\s*!important/);
  assert.match(physicalInput, /background:\s*transparent\s*!important/);
});

test("owned horizontal forms align labels and controls until genuinely narrow width", () => {
  const wide = declarationsForRuleContaining(
    ".sdsync-settings-panel > .sdsync-form-item",
    ".sdsync-horizontal-form > .sdsync-form-item",
    ".sdsync-app .sdsync-inline-form-item"
  );
  assert.match(wide, /grid-template-columns:\s*minmax\(145px, 190px\) minmax\(260px, 1fr\)\s*!important/);
  assert.match(wide, /align-items:\s*center\s*!important/);
  assert.match(wide, /gap:\s*14px\s*!important/);
  assert.match(css, /\.sdsync-app \.sdsync-form-grid > \.sdsync-form-item:not\(\.sdsync-inline-form-item\)/,
    "generic grid rows must exclude the explicitly horizontal routine and security rows");
  assert.match(css, /\.sdsync-app form\.sdsync-panel:not\(\.sdsync-settings-panel\) > \.sdsync-form-item:not\(\.sdsync-inline-form-item\)/,
    "generic panel rows must not override explicitly horizontal Doctor and alert rows");
  assert.doesNotMatch(css, /\.sdsync-app \.sdsync-form-grid > \.sdsync-form-item\s*,/,
    "a higher-specificity generic grid rule would force requested inline fields back to one column");

  const controlColumn = declarationsForRuleContainingDeclaration(
    /grid-column:\s*2\s*!important/,
    ".sdsync-settings-panel > .sdsync-form-item > .sdsync-form-control-cell",
    ".sdsync-horizontal-form > .sdsync-form-item > .sdsync-form-control-cell"
  );
  assert.match(controlColumn, /grid-row:\s*1\s*!important/);
  assert.doesNotMatch(css, /\.sdsync-form-item\s*>\s*:first-child/,
    "control placement must not depend on undocumented DSM child order");

  const shortViewport = css.slice(css.indexOf("@media (max-width: 520px)"));
  assert.match(
    shortViewport,
    /\.sdsync-settings-panel > \.sdsync-form-item,[\s\S]*?\.sdsync-horizontal-form \.sdsync-inline-form-item[\s\S]*?grid-template-columns:\s*minmax\(0, 1fr\)\s*!important/
  );
  const shortOwnedForm = css.slice(css.indexOf(".sdsync-settings-panel.sdsync-compact-form"));
  assert.ok(shortOwnedForm.length < css.length, "missing Chrome 88-compatible AppWindow width fallback");
  assert.match(
    shortOwnedForm,
    /\.sdsync-settings-panel\.sdsync-compact-form > \.sdsync-form-item,[\s\S]*?\.sdsync-horizontal-form\.sdsync-compact-form \.sdsync-inline-form-item[\s\S]*?grid-template-columns:\s*minmax\(0, 1fr\)\s*!important/
  );
  assert.match(shortOwnedForm, /grid-column:\s*1\s*!important[\s\S]*?grid-row:\s*2\s*!important/);
  assert.match(controlLayout, /FORM_COMPACT_WIDTH = 520/);
  assert.match(controlLayout, /width <= FORM_COMPACT_WIDTH/);
  assert.match(controlLayout, /RESPONSIVE_FORM_SELECTOR = "\.sdsync-settings-panel, \.sdsync-horizontal-form, \.sdsync-editor"/);
  assert.match(shortOwnedForm, /\.sdsync-editor\.sdsync-compact-form \.sdsync-form-item/,
    "profile editors must stack by their own AppWindow track width");
});

test("profile catalog and editor are exclusive full-width views with one configuration field per line", () => {
  assert.match(
    app,
    /:class="\['sdsync-profiles-layout', profileEditorOpen \? 'is-editor-only' : 'is-catalog-only'\]"/
  );
  assert.match(
    app,
    /<transition name="sdsync-page-swap" mode="out-in">\s*<div v-if="!profileEditorOpen" key="profile-catalog"[^>]*\bsdsync-profile-catalog\b/
  );
  assert.match(
    app,
    /<v-form v-else key="profile-editor"[^>]*\bsdsync-profile-editor\b/,
    "the editor must replace the catalog instead of opening as its second column"
  );
  assert.doesNotMatch(app, /<v-form v-if="profileEditorOpen"[^>]*\bsdsync-profile-editor\b/,
    "the profile catalog and editor must not render simultaneously");

  const editorTrack = declarationsForRuleContaining(
    ".sdsync-app .sdsync-profiles-layout.is-editor-only",
    ".sdsync-app .sdsync-profiles-layout.is-editor-only > .sdsync-profile-editor"
  );
  assert.match(editorTrack, /width:\s*100%/);
  assert.match(editorTrack, /max-width:\s*100%/);
  assert.match(editorTrack, /min-width:\s*0/);

  const profileSections = declarationsForRuleContaining(
    ".sdsync-app .sdsync-profile-editor > .sdsync-form-grid",
    ".sdsync-app .sdsync-profile-editor .sdsync-advanced > .sdsync-form-grid",
    ".sdsync-app .sdsync-profile-editor > .sdsync-danger-fieldset",
    ".sdsync-app .sdsync-profile-editor > .sdsync-secret-fieldset"
  );
  assert.match(profileSections, /grid-template-columns:\s*minmax\(0, 1fr\)/);
  assert.match(profileSections, /width:\s*100%/);
  assert.match(profileSections, /min-width:\s*0/);

  const profileRows = declarationsForRuleContaining(
    ".sdsync-app .sdsync-profile-editor > .sdsync-form-grid > *",
    ".sdsync-app .sdsync-profile-editor .sdsync-advanced > .sdsync-form-grid > *",
    ".sdsync-app .sdsync-profile-editor > .sdsync-danger-fieldset > *",
    ".sdsync-app .sdsync-profile-editor > .sdsync-secret-fieldset > *"
  );
  assert.match(profileRows, /grid-column:\s*1 \/ -1/);
  assert.match(profileRows, /max-width:\s*100%/);
  assert.match(profileRows, /min-width:\s*0/);

  const editorStart = app.indexOf('<v-form v-else key="profile-editor"');
  const editorEnd = app.indexOf("</v-form>", editorStart);
  const profileEditor = app.slice(editorStart, editorEnd);
  assert.ok(editorStart >= 0 && editorEnd > editorStart, "profile editor template is missing");
  assert.match(profileEditor, /class="sdsync-danger-fieldset"/);
  assert.match(profileEditor, /class="sdsync-advanced"/);
  assert.match(profileEditor, /class="sdsync-toggle-row(?: [^"]*)?"/);
});

test("critical form layout has an explicit Chrome 88 compatibility path", () => {
  assert.match(webpack, /targets:\s*\{\s*chrome:\s*["']88["']\s*\}/);
  assert.doesNotMatch(css, /:has\(/, "critical layout must not require relational selectors unavailable in Chrome 88");

  const progressiveStart = css.indexOf("@container (max-width: 520px)");
  assert.ok(progressiveStart > 0, "container-query enhancement is missing");
  const baselineCss = css.slice(0, progressiveStart);
  assert.match(baselineCss, /\.sdsync-app \.sdsync-control-shell\s*\{/);
  assert.match(baselineCss, /\.sdsync-settings-panel\.sdsync-compact-form > \.sdsync-form-item/);
  assert.match(baselineCss, /> \.sdsync-form-control-cell/);

  assert.match(controlLayout, /classList\.add\("sdsync-control-shell", typeClass\)/);
  assert.match(controlLayout, /classList\.add\("sdsync-control-owner"\)/);
  assert.match(controlLayout, /classList\.add\("sdsync-semantic-control"\)/);
  assert.match(controlLayout, /classList\.add\("sdsync-form-control-cell"\)/);
  assert.match(controlLayout, /classList\.add\("sdsync-form-control-shell"\)/);
  assert.match(controlLayout, /classList\.add\("sdsync-checkbox-label"\)/);
  assert.match(controlLayout, /classList\.add\("sdsync-checkbox-input"\)/);
  assert.match(controlLayout, /classList\.add\("sdsync-checkbox-glyph"\)/);
  assert.match(controlLayout, /classList\.add\("sdsync-select-row"\)/);
  assert.match(controlLayout, /classList\.add\("sdsync-select-prefix"\)/);
  assert.match(controlLayout, /classList\.add\("sdsync-select-affordance"\)/);
  assert.match(controlLayout, /classList\.toggle\([\s\S]*?"sdsync-compact-form"/);
  assert.match(controlLayout, /classList\.toggle\("sdsync-medium-shell", width <= SHELL_MEDIUM_WIDTH\)/);
  assert.match(controlLayout, /classList\.toggle\("sdsync-compact-shell", width <= SHELL_COMPACT_WIDTH\)/);
  assert.match(controlLayout, /OWNED_OVERLAY_SELECTOR = "\.sdsync-select-dropdown"/);
  assert.match(controlLayout, /OVERLAY_STYLE_PROPERTIES = \[[\s\S]*"width"/);
  assert.match(controlLayout, /setImportantStyle\(overlay, "width", `\$\{boundedWidth\}px`\)/);
  assert.match(controlLayout, /overlayMutationObserver\.observe\(document\.body/);
  assert.match(controlLayout, /restoreOverlay\(overlay, overlayOriginalStyles\)/);
  assert.match(controlLayout, /new ResizeObserver\(/);
  assert.match(app, /installControlLayout\(this\.\$el\)/);
  assert.match(app, /this\.controlLayoutCleanup\(\)/);
});

test("secret replacement controls have explicit wide and compact grid placement", () => {
  for (const marker of [
    "sdsync-secret-summary", "sdsync-secret-mode", "sdsync-secret-mode-help",
    "sdsync-secret-value", "sdsync-secret-value-help"
  ]) {
    assert.ok((app.match(new RegExp(marker, "g")) || []).length >= 3,
      `every secret editor must expose ${marker}`);
  }
  assert.match(css, /\.sdsync-secret-summary\s*\{[\s\S]*?grid-column:\s*1;[\s\S]*?grid-row:\s*1 \/ span 2;/);
  assert.match(css, /\.sdsync-secret-mode\s*\{[\s\S]*?grid-column:\s*2;[\s\S]*?grid-row:\s*1;/);
  assert.match(css, /\.sdsync-secret-mode-help\s*\{[\s\S]*?grid-column:\s*3;[\s\S]*?grid-row:\s*1;/);
  assert.match(css, /\.sdsync-secret-value\s*\{[\s\S]*?grid-column:\s*2;[\s\S]*?grid-row:\s*2;/);
  assert.match(css, /\.sdsync-secret-value-help\s*\{[\s\S]*?grid-column:\s*3;[\s\S]*?grid-row:\s*2;/);
  assert.match(css, /\.sdsync-app\.sdsync-compact-shell \.sdsync-secret-editor\s*\{[\s\S]*?grid-template-columns:\s*minmax\(0, 1fr\) 20px;/);
  assert.match(css, /\.sdsync-app\.sdsync-compact-shell \.sdsync-secret-value\s*\{[\s\S]*?grid-column:\s*1;[\s\S]*?grid-row:\s*3;/);
});

test("requested DSM policy, routine, Doctor, and alert fields use SDK horizontal contracts", () => {
  const appLabels = [
    "Profile", "Action", "Mode", "Interval (seconds)", "Window starts", "Window ends",
    "Realtime debounce (seconds)", "Fallback poll (seconds)", "Retry attempts",
    "Retry backoff (seconds)", "Scope", "Failures before alert", "Cooldown (seconds)"
  ];
  const securityLabels = [
    "Policy version", "CSRF lifetime (seconds)", "Result retention (seconds)",
    "Maximum outstanding jobs"
  ];

  for (const [source, labels] of [[app, appLabels], [security, securityLabels]]) {
    const tags = openingTags(source, "v-form-item");
    for (const label of labels) {
      const tag = tags.find((candidate) => candidate.includes(`label="${label}"`));
      assert.ok(tag, `missing requested inline field ${label}`);
      assert.match(tag, /class="[^"]*\bsdsync-inline-form-item\b/);
      assert.match(tag, /label-flex="0 0 150px"/);
      assert.match(tag, /control-flex="1 1 auto"/);
    }
  }

  for (const marker of ["sdsync-routine-editor", "sdsync-doctor-form", "sdsync-alert-form"]) {
    const form = openingTags(app, "v-form").find((tag) => tag.includes(marker));
    assert.ok(form, `missing ${marker}`);
    assert.match(form, /direction="horizontal"/);
    assert.match(form, /class="[^"]*\bsdsync-horizontal-form\b/);
  }
  const securityForm = openingTags(security, "v-form")[0];
  assert.match(securityForm, /direction="horizontal"/);
  assert.match(securityForm, /class="[^"]*\bsdsync-horizontal-form\b/);

  for (const label of ["Search", "Category", "Level", "Source", "Lines"]) {
    assert.match(app, new RegExp(`<span class="sdsync-filter-label">${label}<\\/span>`));
  }
  assert.equal((app.match(/class="sdsync-submit-row"/g) || []).length, 3);
  assert.match(app, /class="sdsync-submit-row"[\s\S]{0,500}>Run doctor<\/v-button>/);
  assert.match(app, /class="sdsync-submit-row"[\s\S]{0,700}interfaceOutcomeUnresolved \? 'Save locked' : 'Save session preferences'[\s\S]{0,80}<\/v-button>/);
  assert.match(
    app,
    /class="sdsync-submit-row"[\s\S]{0,650}'Validate and persist the package-level DSM alert policy immediately'[\s\S]{0,450}alertsOutcomeUnresolved \? 'Save locked' : 'Save now'[\s\S]{0,80}<\/v-button>/,
    "the DSM alert policy keeps its bounded action row after adopting autosave copy"
  );

  assert.match(declarationsForExactSelector(".sdsync-routine-fields,\n.sdsync-inline-field-list"), /grid-template-columns:\s*minmax\(0, 1fr\)/);
  assert.match(declarationsForExactSelector(".sdsync-filter-row"), /grid-template-columns:\s*minmax\(88px, 130px\) minmax\(0, 1fr\)/);
  assert.match(declarationsForExactSelector(".sdsync-submit-row"), /margin:\s*16px 0 2px/);
  assert.match(declarationsForExactSelector(".sdsync-submit-row"), /justify-content:\s*flex-end/);
});
