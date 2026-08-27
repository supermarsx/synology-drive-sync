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

test("Routines prioritizes Configured profiles before Package controller", () => {
  const routines = routeSection("routines");
  const definitions = tabDefinitions(app, "routineTabs", "Routines");
  const labels = tabContract(routines, "Routines", definitions, "routineTabs");
  assert.deepEqual(labels, ["Configured profiles", "Package controller"]);
  assert.match(app, /routineTab:\s*"configured-profiles"/, "Configured profiles must be the initial routine view");
  assertTabLabelsAreNotRepeatedAsHeadings(routines, labels, "Routines");
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
