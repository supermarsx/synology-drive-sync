// Load App.vue's <script> as a plain component options object, wiring every
// `import ... from "./x"` to the real module, so a test exercises the methods and
// computed properties the AppWindow ships rather than a hand-written copy.
// SecurityPanel.vue is the one import that needs a bundler; it becomes `{}`.
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";

const SRC = new URL("../../src/", import.meta.url);
export const appSource = readFileSync(new URL("App.vue", SRC), "utf8");

function resolveLocal(specifier) {
  for (const candidate of [specifier, `${specifier}.js`, `${specifier}.mjs`]) {
    const url = new URL(candidate, SRC);
    if (existsSync(fileURLToPath(url))) return url;
  }
  throw new Error(`load-app: cannot resolve ${specifier}`);
}

// package.json declares no module type, so a .js file is CommonJS to Node. The
// bundler reads them as ES modules; the one-off data: import does the same here.
// (They import nothing, so no relative specifier needs resolving.)
function moduleHref(url) {
  if (!url.pathname.endsWith(".js")) return url.href;
  const text = readFileSync(fileURLToPath(url), "utf8");
  if (/^import\s/m.test(text)) throw new Error(`load-app: ${url.pathname} imports; use .mjs`);
  return `data:text/javascript;base64,${Buffer.from(text).toString("base64")}`;
}

const importCache = new Map();

export async function loadApp({ overrides = {}, exports = "" } = {}) {
  const script = appSource.match(/<script>\s*([\s\S]*?)\s*<\/script>/)[1];
  const stubs = {};
  const importPattern = /^import\s+([\s\S]*?)\s+from\s+"(\.\/[^"]+)";\s*$/gm;
  const bindings = [];
  for (const match of script.matchAll(importPattern)) bindings.push({ clause: match[1].trim(), specifier: match[2] });
  for (const { clause, specifier } of bindings) {
    if (specifier.endsWith(".vue")) {
      stubs[clause] = {};
      continue;
    }
    const url = moduleHref(resolveLocal(specifier));
    if (!importCache.has(url)) importCache.set(url, await import(url));
    const namespace = importCache.get(url);
    if (clause.startsWith("{")) {
      for (const part of clause.slice(1, -1).split(",").map((item) => item.trim()).filter(Boolean)) {
        const [imported, local = imported] = part.split(/\s+as\s+/);
        if (!(imported in namespace)) throw new Error(`load-app: ${specifier} does not export ${imported}`);
        stubs[local] = namespace[imported];
      }
    } else {
      stubs[clause] = namespace.default;
    }
  }
  const executable = script
    .replace(importPattern, "")
    .replace("export default {", "const AppComponent = {")
    + (exports ? `\nreturn { AppComponent, ${exports} };` : "\nreturn AppComponent;");
  Object.assign(stubs, overrides);
  return Function(...Object.keys(stubs), executable)(...Object.values(stubs));
}

/**
 * A stand-in `this` for a component method: every computed property is a live
 * getter, every method is bound, and `state` supplies the data a test cares
 * about. Only what `state` names exists, so a method that reads data the test
 * did not supply fails loudly instead of reading a default.
 */
export function appContext(component, state = {}) {
  const context = { toasts: [], ...state };
  if (!("toast" in state)) context.toast = (title, message, error) => context.toasts.push({ title, message, error });
  for (const name of Object.keys(component.computed || {})) {
    if (Object.prototype.hasOwnProperty.call(context, name)) continue;
    Object.defineProperty(context, name, { get: () => component.computed[name].call(context), configurable: true });
  }
  for (const name of Object.keys(component.methods || {})) {
    if (!(name in context)) context[name] = (...args) => component.methods[name].call(context, ...args);
  }
  return context;
}

export const moduleUrl = (name) => pathToFileURL(fileURLToPath(new URL(name, SRC))).href;
