// A test that runs a browser must never put one on the user's desktop. On Windows
// chrome.exe and msedge.exe ignore `--version` and open a window, so a probe by
// execution did exactly that on every `node --test`. This reads the test sources
// and fails if one spawns a browser without --headless and a throwaway profile, or
// probes a candidate by executing it outside helpers/browser.mjs.
import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import test from "node:test";

const directory = new URL("./", import.meta.url);
const files = (await readdir(directory)).filter((name) => name.endsWith(".mjs"))
  .concat((await readdir(new URL("./helpers/", directory))).filter((name) => name.endsWith(".mjs")).map((name) => `helpers/${name}`));
const sources = new Map(await Promise.all(files.map(async (name) => [name, await readFile(new URL(name, directory), "utf8")])));

// First arguments that are tools, not browsers.
const NON_BROWSER_TOOLS = new Set(["powershell.exe", "pkill", "taskkill"]);

function spawnCalls(source) {
  const calls = [];
  for (const match of source.matchAll(/\b(?:spawnSync|spawn|execFileSync|execFile|execSync|exec)\(\s*([^,)]+)/g)) {
    calls.push(match[1].trim());
  }
  return calls;
}

test("every test that spawns a browser passes --headless and a throwaway --user-data-dir", () => {
  const spawners = [];
  for (const [name, source] of sources) {
    // The helper's one execution is the non-Windows --version probe, checked below.
    if (name === "browser-spawn-safety.test.mjs" || name === "helpers/browser.mjs") continue;
    const browserCalls = spawnCalls(source).filter((argument) => {
      const literal = argument.match(/^["'`](.*)["'`]$/);
      return !(literal && NON_BROWSER_TOOLS.has(literal[1]));
    });
    if (!browserCalls.length) continue;
    spawners.push(name);
    assert.ok(source.includes('"--headless=new"') || source.includes("'--headless=new'"), `${name} spawns ${browserCalls.join(", ")} without --headless=new`);
    assert.match(source, /--user-data-dir=\$\{/, `${name} spawns a browser without a throwaway --user-data-dir`);
  }
  assert.ok(spawners.includes("rendered-control-layout.test.mjs") && spawners.includes("action-spinner-contract.test.mjs"),
    `the scan found no browser spawners (${spawners}); it is looking at the wrong thing`);
});

test("no test probes a browser by running --version except the helper, which never does on Windows", () => {
  for (const [name, source] of sources) {
    if (name === "browser-spawn-safety.test.mjs" || name === "helpers/browser.mjs") continue;
    assert.ok(!/["']--version["']/.test(source), `${name} executes a browser to probe it; use helpers/browser.mjs`);
  }
  const helper = sources.get("helpers/browser.mjs");
  const probeAt = helper.indexOf('["--version"]');
  const winBranch = helper.indexOf('process.platform === "win32"', helper.indexOf("export function findBrowser"));
  assert.ok(probeAt > 0 && winBranch > 0 && winBranch < probeAt, "the Windows branch must return before the --version probe");
  assert.match(helper.slice(winBranch, probeAt), /continue;/, "the Windows branch must skip, never fall through to the probe");
});
