import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const iconSourceUrl = new URL("../src/ActionIcon.js", import.meta.url);
const cssSourceUrl = new URL("../src/styles/native.css", import.meta.url);
const cssDistUrl = new URL("../dist/style.css", import.meta.url);

async function loadActionIcon() {
  const source = await readFile(iconSourceUrl, "utf8");
  return import(`data:text/javascript;base64,${Buffer.from(source).toString("base64")}#${Date.now()}-${Math.random()}`);
}

function ruleAfter(css, marker) {
  const start = css.indexOf(marker);
  assert.notEqual(start, -1, `missing CSS marker ${marker}`);
  const open = css.indexOf("{", start);
  const close = css.indexOf("}", open);
  assert.ok(open > start && close > open, `missing CSS block after ${marker}`);
  return css.slice(open + 1, close);
}

function assertSpinnerCss(css, label) {
  const buttonIcons = ruleAfter(
    css,
    ".sdsync-app button:not(.sdsync-nav-item):not(.sdsync-profile-row):not(.sdsync-routine-row) > .sdsync-action-icon,"
  );
  assert.match(buttonIcons, /width:\s*15px\s*!important/);
  assert.match(buttonIcons, /height:\s*15px\s*!important/);
  assert.match(buttonIcons, /flex:\s*0 0 15px\s*!important/);
  assert.doesNotMatch(
    buttonIcons,
    /transform:\s*none\s*!important/,
    `${label} button normalization must not suppress SVG transform animation`,
  );

  const spinner = ruleAfter(css, ".sdsync-app .sdsync-action-icon.sdsync-is-spinning");
  assert.match(spinner, /animation:\s*sdsync-spin 0\.8s linear infinite/);
  assert.match(spinner, /transform-origin:\s*center/);
  assert.match(spinner, /transform-box:\s*fill-box/);
  assert.match(spinner, /will-change:\s*transform/);
  assert.match(css, /@keyframes sdsync-spin\s*\{\s*from\s*\{\s*transform:\s*rotate\(0deg\)/);
  assert.match(css, /to\s*\{\s*transform:\s*rotate\(360deg\)/);

  const reducedStart = css.indexOf("@media (prefers-reduced-motion: reduce)");
  assert.notEqual(reducedStart, -1, `${label} lacks a reduced-motion contract`);
  const reduced = css.slice(reducedStart);
  assert.match(
    reduced,
    /\.sdsync-app \.sdsync-action-icon\.sdsync-is-spinning\s*\{[^}]*animation:\s*none\s*!important[^}]*transform:\s*none\s*!important[^}]*stroke-dasharray:\s*3 2/s,
  );
  assert.match(
    reduced,
    /\.sdsync-live-operation-indicator\s*\{[^}]*border-style:\s*dashed[^}]*box-shadow:\s*inset 3px 0 0 var\(--sdsync-fire\)/s,
  );
}

test("functional ActionIcon forwards caller classes and presentation data", async () => {
  const { ActionIcon } = await loadActionIcon();
  const nodes = [];
  const createElement = (tag, data, children = []) => {
    const node = { tag, data, children };
    nodes.push(node);
    return node;
  };
  const dynamicClass = { "sdsync-is-spinning": true, "is-muted": false };
  const dynamicStyle = { opacity: 0.75 };
  const rendered = ActionIcon.render(createElement, {
    props: { name: "refresh", size: 22 },
    data: {
      staticClass: "caller-static",
      class: dynamicClass,
      staticStyle: { color: "red" },
      style: dynamicStyle,
      attrs: { "data-contract": "forwarded", width: 999 },
      on: { click() {} },
      props: { name: "refresh", size: 22 }
    }
  });

  assert.equal(rendered.tag, "svg");
  assert.equal(rendered.data.staticClass, "caller-static");
  assert.deepEqual(rendered.data.class, ["sdsync-action-icon", dynamicClass]);
  assert.deepEqual(rendered.data.staticStyle, { color: "red" });
  assert.strictEqual(rendered.data.style[1], dynamicStyle);
  assert.equal(rendered.data.attrs["data-contract"], "forwarded");
  assert.equal(rendered.data.attrs.width, 22, "component size remains authoritative");
  assert.equal(rendered.data.attrs["aria-hidden"], "true");
  assert.equal(rendered.data.props, undefined, "component-only props must not leak onto the SVG");
  assert.equal(typeof rendered.data.on.click, "function");
  assert.ok(nodes.filter((node) => node.tag === "path").length >= 2);
});

test("source and built CSS keep busy icons rotating without layout shift", async () => {
  const [source, dist] = await Promise.all([
    readFile(cssSourceUrl, "utf8"),
    readFile(cssDistUrl, "utf8")
  ]);
  assertSpinnerCss(source, "source CSS");
  assertSpinnerCss(dist, "built CSS");
});
