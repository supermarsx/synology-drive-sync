// Finding a headless-capable Chrome or Chromium for the rendered-layout tests.
//
// On Windows chrome.exe and msedge.exe ignore `--version`: they start the browser
// and open a window or a tab (in the user's own running profile, for Edge). So on
// Windows a candidate is chosen by whether its file exists and is never executed
// to probe it. Elsewhere `--version` prints and exits, and is the right probe.
// Every actual render passes `--headless=new` and a throwaway `--user-data-dir`
// (see browser-spawn-safety.test.mjs, which fails if a test does otherwise).
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";

export function browserCandidates() {
  const configured = [process.env.CHROME_BIN, process.env.GOOGLE_CHROME_BIN].filter(Boolean);
  if (process.platform === "win32") {
    return configured.concat([
      "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
      "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
      "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
      "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe"
    ]);
  }
  if (process.platform === "darwin") {
    return configured.concat([
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
      "google-chrome",
      "chromium"
    ]);
  }
  return configured.concat(["google-chrome-stable", "google-chrome", "chromium", "chromium-browser"]);
}

export function findBrowser() {
  for (const candidate of browserCandidates()) {
    const isPath = candidate.includes("/") || candidate.includes("\\");
    if (process.platform === "win32") {
      // Never execute a candidate to probe it here. A bare name cannot be resolved
      // without running it, so it is skipped; set CHROME_BIN to a full path.
      if (isPath && existsSync(candidate)) return candidate;
      continue;
    }
    if (isPath) {
      if (existsSync(candidate)) return candidate;
      continue;
    }
    const probe = spawnSync(candidate, ["--version"], { encoding: "utf8", timeout: 5000, windowsHide: true });
    if (!probe.error && probe.status === 0) return candidate;
  }
  return null;
}
