"use strict";
/* Serves the real built AppWindow bundle plus a mock package API on loopback, opens it in headless
 * Chromium at 1280x800, and writes a PNG. Runs inside Docker only (see ../generate.sh).
 *
 *   node shoot.js DIST_DIR OUT.png [route]
 */
const fs = require("fs");
const http = require("http");
const path = require("path");
const puppeteer = require("puppeteer-core");
const demo = require("./demo-data.js");

const [distDir, outPng, route = "overview"] = process.argv.slice(2);
const here = __dirname;
const iconSvg = process.env.UI_ICON || "";
const types = { ".js": "text/javascript", ".css": "text/css", ".html": "text/html", ".svg": "image/svg+xml" };

function send(res, status, body, type) {
  res.writeHead(status, { "content-type": type, "cache-control": "no-store" });
  res.end(body);
}
function json(res, status, value) { send(res, status, JSON.stringify(value), "application/json"); }

const server = http.createServer((req, res) => {
  const url = new URL(req.url, "http://127.0.0.1");
  if (url.pathname === "/") return send(res, 200, fs.readFileSync(path.join(here, "page.html")), types[".html"]);
  if (url.pathname === "/vue.js") return send(res, 200, fs.readFileSync(require.resolve("vue/dist/vue.min.js")), types[".js"]);
  if (url.pathname === "/dsm-stubs.js") return send(res, 200, fs.readFileSync(path.join(here, "dsm-stubs.js")), types[".js"]);
  if (url.pathname.startsWith("/dist/")) {
    const file = path.join(distDir, path.basename(url.pathname));
    return fs.existsSync(file) ? send(res, 200, fs.readFileSync(file), types[path.extname(file)] || "text/plain") : send(res, 404, "", "text/plain");
  }
  if (url.pathname.startsWith("/webman/3rdparty/synology-drive-sync/images/")) {
    return iconSvg && fs.existsSync(iconSvg) ? send(res, 200, fs.readFileSync(iconSvg), types[".svg"]) : send(res, 404, "", "text/plain");
  }
  if (url.pathname === "/webapi/entry.cgi") return json(res, 200, { success: true, data: { synotoken: "demo-synotoken" } });
  if (url.pathname === "/webman/3rdparty/synology-drive-sync/api.cgi") {
    const action = url.searchParams.get("action");
    if (action === "snapshot") return json(res, 200, demo.snapshot());
    if (action === "csrf") return json(res, 200, demo.csrf());
    if (action === "activity") return json(res, 200, { schema: "sdsync.dsm-activity.v1", ok: true, events: [] });
    if (action === "logs") return json(res, 200, { schema: "sdsync.dsm-logs.v1", ok: true, lines: [] });
    return json(res, 404, { schema: "sdsync.dsm-error.v1", ok: false, code: "demo_unavailable", message: "Not part of the demo.", status: 404 });
  }
  return send(res, 404, "", "text/plain");
});

server.listen(0, "127.0.0.1", async () => {
  const port = server.address().port;
  const browser = await puppeteer.launch({
    executablePath: process.env.CHROME_BIN,
    headless: "new",
    args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage", "--hide-scrollbars", "--force-color-profile=srgb", "--font-render-hinting=none"]
  });
  try {
    const page = await browser.newPage();
    await page.setViewport({ width: 1280, height: 800, deviceScaleFactor: 1 });
    await page.emulateTimezone("UTC");
    const problems = [];
    page.on("pageerror", (error) => problems.push(`pageerror: ${error.message}`));
    page.on("console", (message) => { if (["error", "warning"].includes(message.type())) problems.push(`console.${message.type()}: ${message.text()}`); });
    // Pretend the clock reads the demo moment, so "Updated 09:41:12" and the dates are the demo's.
    await page.evaluateOnNewDocument((demoMs) => {
      const realNow = Date.now.bind(Date);
      const offset = demoMs - realNow();
      const RealDate = Date;
      class FakeDate extends RealDate {
        constructor(...args) { if (args.length === 0) super(realNow() + offset); else super(...args); }
        static now() { return realNow() + offset; }
      }
      window.Date = FakeDate;
    }, demo.NOW * 1000);
    await page.goto(`http://127.0.0.1:${port}/`, { waitUntil: "networkidle0" });
    await page.waitForSelector(".sdsync-metrics", { timeout: 20000 });
    if (route !== "overview") {
      await page.evaluate((id) => { const button = [...document.querySelectorAll(".sdsync-nav-item")].find((item) => item.getAttribute("aria-label") === id); if (button) button.click(); }, route);
    }
    // Let the first poll, the toast and the transitions settle.
    await new Promise((resolve) => setTimeout(resolve, 2500));
    await page.screenshot({ path: outPng });
    if (problems.length) console.error("page problems:\n" + problems.join("\n"));
  } finally {
    await browser.close();
    server.close();
  }
});
