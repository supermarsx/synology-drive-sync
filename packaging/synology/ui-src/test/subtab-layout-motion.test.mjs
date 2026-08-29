import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const app = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const security = await readFile(new URL("../src/SecurityPanel.vue", import.meta.url), "utf8");
const css = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");

function routeSection(route) {
  const marker = `<section v-else-if="route === '${route}'"`;
  const start = app.indexOf(marker);
  assert.notEqual(start, -1, `missing ${route} route section`);
  const next = app.indexOf("<section v-else-if=", start + marker.length);
  return app.slice(start, next === -1 ? app.indexOf("</main>", start) : next);
}

function textContent(markup) {
  return markup
    .replace(/<[^>]+>/g, " ")
    .replace(/\{\{[\s\S]*?\}\}/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

function tabDefinitions(source, constant, owner) {
  const block = source.match(new RegExp(`${constant}:\\s*\\[([\\s\\S]*?)\\]`));
  assert.ok(block, `${owner} needs an explicit ordered ${constant} definition`);
  const definitions = [...block[1].matchAll(
    /\{\s*id:\s*"([^"]+)",\s*label:\s*"([^"]+)"\s*\}/g
  )].map((match) => ({ id: match[1], label: match[2] }));
  assert.ok(definitions.length >= 2, `${owner} needs at least two ordered tab definitions`);
  return definitions;
}

function tabContract(markup, owner, definitions, constant) {
  const tablist = markup.match(
    /<(?:div|nav)\b(?=[^>]*\bclass="[^"]*sdsync-subtabs[^"]*")(?=[^>]*\brole="tablist")[^>]*>[\s\S]*?<\/(?:div|nav)>/
  );
  assert.ok(tablist, `${owner} needs one accessible internal tablist`);

  const tabs = [...tablist[0].matchAll(
    /<button\b(?=[^>]*\bclass="[^"]*sdsync-subtab[^"]*")(?=[^>]*\brole="tab")([^>]*)>([\s\S]*?)<\/button>/g
  )].map((match) => ({ attrs: match[1], label: textContent(match[2]) }));
  assert.equal(tabs.length, 1, `${owner} should render one shared semantic tab template`);
  assert.match(tabs[0].attrs, new RegExp(`\\bv-for="tab in ${constant}"`));

  for (const tab of tabs) {
    assert.match(tab.attrs, /:aria-selected=/, `${owner}/${tab.label} must expose selected state`);
    assert.match(tab.attrs, /:tabindex=/, `${owner}/${tab.label} must expose roving keyboard state`);
    assert.match(tab.attrs, /(?:@click|@keydown)=/, `${owner}/${tab.label} must be interactive`);
  }

  assert.match(markup, /<transition\b[^>]*\bname="sdsync-subtab-swap"[^>]*\bmode="out-in"[^>]*>/,
    `${owner} panels must swap coherently instead of stacking`);
  assert.match(markup, /\bclass="[^"]*sdsync-subtab-stage[^"]*"/);
  assert.match(markup, /\bclass="[^"]*sdsync-subtab-panel[^"]*"[^>]*\brole="tabpanel"/);

  const ownerSlug = owner.toLowerCase();
  for (const definition of definitions) {
    assert.ok(
      markup.includes(`id="sdsync-${ownerSlug}-panel-${definition.id}"`),
      `${owner}/${definition.label} needs a stable owned tabpanel`
    );
    assert.ok(
      markup.includes(`aria-labelledby="sdsync-${ownerSlug}-tab-${definition.id}"`),
      `${owner}/${definition.label} panel must point back to its tab`
    );
  }

  return definitions.map((definition) => definition.label);
}

function assertTabLabelsAreNotRepeatedAsHeadings(markup, labels, owner) {
  const headings = [...markup.matchAll(
    /<(h[1-6]|p)\b([^>]*)>([\s\S]*?)<\/\1>/g
  )]
    .filter((match) => match[1].startsWith("h") || /sdsync-eyebrow/.test(match[2]))
    .map((match) => textContent(match[3]).toLowerCase());

  for (const label of labels) {
    assert.equal(
      headings.includes(label.toLowerCase()),
      false,
      `${owner} repeats the ${JSON.stringify(label)} tab as a redundant panel heading`
    );
  }
}

test("primary route content fades out and in with a reduced-motion escape hatch", () => {
  assert.match(app, /<(?:main|div)\b[^>]*\bclass="[^"]*sdsync-page-stage[^"]*"/);
  assert.match(
    app,
    /<transition\b[^>]*\bname="sdsync-page-swap"[^>]*\bmode="out-in"[^>]*>/,
    "primary route changes must finish leave before enter"
  );
  assert.match(app, /:key="route"/, "the transition needs a route-keyed content owner");

  for (const transition of ["sdsync-page-swap", "sdsync-subtab-swap"]) {
    for (const vue2Phase of ["enter-active", "leave-active", "enter", "enter-to", "leave", "leave-to"]) {
      assert.ok(css.includes(`.${transition}-${vue2Phase}`), `missing Vue 2 ${transition}/${vue2Phase} phase`);
    }
  }

  for (const variable of ["out", "in"]) {
    const duration = css.match(new RegExp(`--sdsync-motion-${variable}:\\s*([0-9.]+)ms`));
    assert.ok(duration, `missing ${variable} transition duration`);
    assert.ok(Number(duration[1]) >= 120, `${variable} transition is too short to remain perceptible`);
  }
  assert.match(
    css,
    /\.sdsync-page-swap-enter-active[\s\S]{0,650}transition:[\s\S]*?opacity[^;]*,[\s\S]*?transform[^;]*;/,
    "enter phase must animate a restrained fade and slide"
  );
  assert.match(
    css,
    /\.sdsync-page-swap-leave-active[\s\S]{0,650}transition:[\s\S]*?opacity[^;]*,[\s\S]*?transform[^;]*;/,
    "leave phase must animate a restrained fade and slide"
  );
  assert.match(css, /\.sdsync-page-swap-enter,[\s\S]{0,450}opacity:\s*0;[\s\S]{0,120}transform:\s*translateY\(5px\)/);
  assert.match(css, /\.sdsync-page-swap-leave-to,[\s\S]{0,250}opacity:\s*0;[\s\S]{0,120}transform:\s*translateY\(-3px\)/);
  assert.match(css, /\.sdsync-page-stage\s*\{[\s\S]{0,120}min-height:/,
    "out-in route motion needs a stable stage so content does not collapse between phases");
  assert.match(css, /\.sdsync-subtab-stage\s*\{[\s\S]{0,120}min-height:/,
    "out-in subtab motion needs a stable stage so content does not collapse between phases");

  const reducedStart = css.indexOf("@media (prefers-reduced-motion: reduce)");
  assert.notEqual(reducedStart, -1, "missing reduced-motion media query");
  const nextMedia = css.indexOf("@media ", reducedStart + 1);
  const reduced = css.slice(reducedStart, nextMedia === -1 ? css.length : nextMedia);
  assert.match(reduced, /\.sdsync-app \*/);
  assert.match(reduced, /transition-duration:\s*0\.01ms\s*!important/);
  assert.match(reduced, /animation-duration:\s*0\.01ms\s*!important/);
  assert.match(reduced, /transform:\s*none\s*!important/);
});

test("Settings uses horizontal native form rows with help adjacent to each label", () => {
  const settings = routeSection("settings");
  assert.match(settings, /<v-form\b[^>]*\bclass="[^"]*sdsync-settings-panel[^"]*"[^>]*\bdirection="horizontal"/);

  for (const [label, helpKey] of [
    ["Theme", "settings-theme"],
    ["Status refresh", "settings-status-refresh"],
    ["Log refresh", "settings-log-refresh"]
  ]) {
    const item = settings.match(new RegExp(
      `<v-form-item\\b(?=[^>]*\\blabel="${label}")[^>]*>([\\s\\S]*?)<\\/v-form-item>`
    ));
    assert.ok(item, `missing horizontal Settings row ${label}`);
    assert.match(
      item[1],
      new RegExp(
        `<template #label-after>\\s*<control-help\\b(?=[^>]*\\bclass="[^"]*sdsync-form-label-help[^"]*")(?=[^>]*\\bhelp-key="${helpKey}")[^>]*\\/>\\s*<\\/template>`
      ),
      `${label} tooltip must use DSM's label-after slot`
    );
    assert.equal((item[1].match(/<control-help\b/g) || []).length, 1,
      `${label} must not duplicate help in the default control slot`);
    assert.match(item[1], /<v-single-select\b/, `${label} needs a front-facing select control`);
    assert.ok(
      item[1].indexOf("#label-after") < item[1].indexOf("<v-single-select"),
      `${label} label/help must precede its control`
    );
  }
});

test("Profiles swaps a full-width catalog and editor as mutually exclusive keyed views", () => {
  const profiles = routeSection("profiles");

  assert.match(
    profiles,
    /<div\b(?=[^>]*\bv-if="!profileEditorOpen")(?=[^>]*\bclass="[^"]*sdsync-page-actions[^"]*")[^>]*>\s*<v-button\b(?=[^>]*@click="openProfile\(''\)")[^>]*>[\s\S]*?New profile<\/v-button>\s*<\/div>/,
    "the New profile action must belong to the catalog view"
  );
  assert.match(
    profiles,
    /:class="\['sdsync-profiles-layout', profileEditorOpen \? 'is-editor-only' : 'is-catalog-only'\]"/,
    "the profile view must expose explicit one-track catalog/editor layout modes"
  );

  const exclusiveViews = profiles.match(
    /<transition\b(?=[^>]*\bname="sdsync-page-swap")(?=[^>]*\bmode="out-in")[^>]*>\s*(<div\b(?=[^>]*\bv-if="!profileEditorOpen")(?=[^>]*\bkey="profile-catalog")(?=[^>]*\bclass="[^"]*sdsync-profile-catalog[^"]*")[^>]*>[\s\S]*?<\/div>)\s*(<v-form\b(?=[^>]*\bv-else\b)(?=[^>]*\bkey="profile-editor")(?=[^>]*\bclass="[^"]*sdsync-editor[^"]*")[^>]*>[\s\S]*?<\/v-form>)\s*<\/transition>/
  );
  assert.ok(exclusiveViews, "catalog v-if and editor v-else must be immediate keyed siblings in one out-in transition");

  const catalog = exclusiveViews[1];
  const editor = exclusiveViews[2];
  assert.match(catalog, /@click="openProfile\(profile\.name\)"/,
    "an existing profile must enter the same dedicated editor view");
  assert.match(editor, /<div class="sdsync-form-grid">/,
    "the dedicated editor must retain the one-field-per-line form grid");
  assert.match(editor, /<v-button\b(?=[^>]*@click="closeProfile")[^>]*>[\s\S]*?Close<\/v-button>/);
  assert.match(editor, /<v-button\b(?=[^>]*@click="closeProfile")[^>]*>[\s\S]*?Cancel<\/v-button>/);
  assert.equal((editor.match(/@click="closeProfile"/g) || []).length, 2,
    "Close and Cancel must share the same catalog-return behavior");
  assert.doesNotMatch(profiles, /sdsync-profile-catalog[\s\S]*?<v-form\b[^>]*\bv-if="profileEditorOpen"/,
    "catalog and editor must never return to the former simultaneous two-column contract");

  const viewModes = css.match(
    /\.sdsync-profiles-layout\.is-catalog-only,[\s\S]*?\.sdsync-profiles-layout\.is-editor-only,[\s\S]*?\{([\s\S]*?)\}/
  );
  assert.ok(viewModes, "profile catalog/editor layout modes need an explicit shared CSS rule");
  assert.match(viewModes[1], /grid-template-columns:\s*minmax\(0, 1fr\)/,
    "both profile views must occupy one full-width grid track");
  const formGrid = css.match(/\.sdsync-form-grid\s*\{([\s\S]*?)\}/);
  assert.ok(formGrid, "profile editor form grid styling is missing");
  assert.match(formGrid[1], /grid-template-columns:\s*minmax\(0, 1fr\)/,
    "profile fields must remain one per line instead of becoming a second editor column");
});

test("Routines uses a profile-style New routine action and a catalog-first editor without redundant subtabs", () => {
  const routines = routeSection("routines");
  const profileAction = routeSection("profiles").match(
    /<div\b(?=[^>]*\bv-if="!profileEditorOpen")(?=[^>]*\bclass="sdsync-page-actions")[^>]*>\s*(<v-button\b[^>]*>[\s\S]*?New profile<\/v-button>)\s*<\/div>/
  );
  const routineAction = routines.match(
    /<div class="sdsync-page-actions">\s*(<v-button\b[^>]*>[\s\S]*?New routine<\/v-button>)\s*<\/div>/
  );
  assert.ok(profileAction, "Profiles needs its primary New profile action");
  assert.ok(routineAction, "Routines needs its primary New routine action");
  for (const action of [profileAction[1], routineAction[1]]) {
    assert.match(action, /suffix="main"/);
    assert.match(action, /display="icon-text"/);
    assert.match(action, /<action-icon name="add"\s*\/>/);
  }
  assert.match(routineAction[1], /@click="openRoutine\(''\)"/);
  assert.match(routines, /:class="\['sdsync-routines-layout', \{ 'is-catalog-only': !routineEditorOpen \}\]"/);
  assert.ok(
    routines.indexOf('class="sdsync-panel sdsync-routine-catalog"')
      < routines.indexOf('v-if="routineEditorOpen"'),
    "the configured routine catalog must precede the conditional editor"
  );
  assert.match(app, /routineEditorOpen:\s*false/);
  assert.match(app, /openRoutine\(profile = ""\) \{[^}]*this\.routineEditorOpen = true; this\.loadRoutine\(profile\); \}/);
  assert.doesNotMatch(routines, /sdsync-subtabs|role="tablist"|role="tabpanel"|data-subtab-panel/);
  assert.doesNotMatch(app, /routineTabs|routineTab:/);
  assert.doesNotMatch(routines, /package-controller|Package controller/);
});

test("Notifications separates DSM policy from open-session behavior into two tabs", () => {
  const notifications = routeSection("notifications");
  const definitions = tabDefinitions(app, "notificationTabs", "Notifications");
  const labels = tabContract(notifications, "Notifications", definitions, "notificationTabs");
  assert.equal(labels.length, 2, "Notifications must have exactly two coherent subtabs");
  assert.match(labels[0], /DSM|package|alert/i, "the first Notifications tab must own DSM/package alerts");
  assert.match(labels[1], /browser|session/i, "the second Notifications tab must own open-session behavior");
  assert.notEqual(labels[0], labels[1]);
  assert.match(app, /notificationTab:\s*"package-alerts"/, "Package alerts must be the initial notification view");
  assertTabLabelsAreNotRepeatedAsHeadings(notifications, labels, "Notifications");
});

test("Security leads with settings and defers structured observability resources", () => {
  const definitions = tabDefinitions(security, "securityTabs", "Security");
  const labels = tabContract(security, "Security", definitions, "securityTabs");
  assert.equal(labels.length, 2, "Security must have exactly two coherent subtabs");
  assert.deepEqual(definitions.map((tab) => tab.id), ["policy-controls", "observability-limits"]);
  assert.match(labels[0], /permissions|settings|risk/i, "Security settings must remain the primary tab");
  assert.match(labels[1], /observability|bounded|limits/i);
  assert.match(security, /securityTab:\s*"policy-controls"/, "policy controls must be the initial Security view");
  const observability = security.slice(security.indexOf('id="sdsync-security-panel-observability-limits"'));
  assert.match(observability, /Structured observability/);
  assert.match(observability, /Bounded resources/);
  assertTabLabelsAreNotRepeatedAsHeadings(security, labels, "Security");
});
