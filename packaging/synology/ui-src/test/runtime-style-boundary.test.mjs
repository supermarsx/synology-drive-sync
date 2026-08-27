import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const main = await readFile(new URL("../src/main.js", import.meta.url), "utf8");
const runtimeStyles = await readFile(new URL("../src/runtimeStyles.js", import.meta.url), "utf8");
const webpack = await readFile(new URL("../webpack.config.js", import.meta.url), "utf8");

test("the executed JavaScript bundle installs its matching CSS after DSM cached links", () => {
  assert.match(main, /import runtimeCss from "\.\/styles\/native\.css\?runtime"/);
  assert.match(main, /installRuntimeStyles\(runtimeCss\)/);
  assert.match(webpack, /resourceQuery:\s*\/runtime\//);
  assert.match(webpack, /type:\s*"asset\/source"/);

  const appended = [];
  const elements = new Map();
  const head = {
    appendChild(element) {
      appended.push(element);
      elements.set(element.id, element);
    }
  };
  const document = {
    head,
    documentElement: head,
    getElementById(id) { return elements.get(id) || null; },
    createElement(tag) {
      assert.equal(tag, "style");
      return {
        id: "",
        textContent: "",
        setAttribute(name, value) { this[name] = value; }
      };
    }
  };
  const context = vm.createContext({ document });
  vm.runInContext(
    `${runtimeStyles.replace(/^export\s+/gm, "")}\nthis.installRuntimeStyles = installRuntimeStyles;`,
    context
  );

  const first = context.installRuntimeStyles(".current { color: #ff5a1f; }");
  const second = context.installRuntimeStyles(".newer { color: #ff2f0f; }");
  assert.equal(first, second, "route remounts must reuse one authoritative style element");
  assert.equal(appended.length, 1);
  assert.equal(second.textContent, ".newer { color: #ff2f0f; }");
  assert.equal(second["data-sdsync-runtime-style"], "current");
});
