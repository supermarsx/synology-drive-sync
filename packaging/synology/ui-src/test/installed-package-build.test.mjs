import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

// The About page names the installed package build (armv7, armv8, ...) from the
// snapshot's package.target and package.arch. The function under test is sliced
// out of App.vue and loaded against the real boundedText from api.js, so what is
// proven is the shipped text, not a copy of it.
const source = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const apiUrl = new URL("../src/api.js", import.meta.url).href;

function sourceSlice(start, end) {
  const from = source.indexOf(start);
  const to = source.indexOf(end, from);
  assert.ok(from >= 0 && to > from, `missing source slice ${start} -> ${end}`);
  return source.slice(from, to);
}

async function loadBuildLabel() {
  const helper = sourceSlice("const PACKAGE_BUILD_UNREPORTED", "function defaultSecurityPolicy");
  const moduleSource = `
    import { boundedText } from ${JSON.stringify(apiUrl)};
    ${helper}
    export { PACKAGE_BUILD_UNREPORTED, installedPackageBuildLabel };
  `;
  return import(`data:text/javascript;base64,${Buffer.from(moduleSource).toString("base64")}#${Date.now()}-${Math.random()}`);
}

const snapshotWith = (pkg) => ({ schema: "sdsync.dsm-api.v1", package: pkg });

test("a package build shows its Synology architecture and the target it was compiled for", async () => {
  const { installedPackageBuildLabel } = await loadBuildLabel();
  for (const [arch, target] of [
    ["x86_64", "x86_64-unknown-linux-musl"],
    ["i686", "i686-unknown-linux-musl"],
    ["armv7", "armv7-unknown-linux-musleabihf"],
    ["armv8", "aarch64-unknown-linux-musl"]
  ]) {
    assert.equal(
      installedPackageBuildLabel(snapshotWith({ version: "26.60", target, arch })),
      `${arch} (${target})`
    );
  }
});

test("a target with no Synology architecture is named as not a package build, never guessed", async () => {
  const { installedPackageBuildLabel } = await loadBuildLabel();
  assert.equal(
    installedPackageBuildLabel(snapshotWith({ version: "26.60", target: "x86_64-unknown-linux-gnu", arch: null })),
    "Not a Synology package build (x86_64-unknown-linux-gnu)"
  );
  // An empty or non-string arch is not an architecture either.
  for (const arch of ["", 8, {}, [], true]) {
    assert.equal(
      installedPackageBuildLabel(snapshotWith({ target: "aarch64-apple-darwin", arch })),
      "Not a Synology package build (aarch64-apple-darwin)",
      JSON.stringify(arch)
    );
  }
});

test("an older bridge that reports neither falls back to the version row's own wording", async () => {
  const { installedPackageBuildLabel, PACKAGE_BUILD_UNREPORTED } = await loadBuildLabel();
  assert.equal(PACKAGE_BUILD_UNREPORTED, "Not reported by package API");
  for (const snapshot of [
    null,
    undefined,
    {},
    { package: null },
    { package: "armv8" },
    { package: [] },
    snapshotWith({ version: "26.51" }),
    snapshotWith({ version: "26.51", target: "", arch: "" }),
    snapshotWith({ target: 7, arch: 8 })
  ]) {
    assert.equal(installedPackageBuildLabel(snapshot), PACKAGE_BUILD_UNREPORTED, JSON.stringify(snapshot));
  }
});

test("hostile and overlong values are bounded text, not markup and not unbounded", async () => {
  const { installedPackageBuildLabel } = await loadBuildLabel();
  const hostile = installedPackageBuildLabel(
    snapshotWith({ target: "<img src=x onerror=alert(1)>", arch: "<script>alert(1)</script>" })
  );
  // The label is a plain string the template interpolates, which Vue escapes. The
  // contract here is that it is passed through verbatim as text and not reshaped.
  assert.equal(typeof hostile, "string");
  assert.equal(hostile, "<script>alert(1)</script> (<img src=x onerror=alert(1)>)");

  const long = "a".repeat(100000);
  const bounded = installedPackageBuildLabel(snapshotWith({ target: long, arch: long }));
  assert.ok(bounded.length <= 128 + " (".length + 128 + ")".length, `label is ${bounded.length} characters`);
  const boundedTargetOnly = installedPackageBuildLabel(snapshotWith({ target: long, arch: null }));
  assert.ok(boundedTargetOnly.length < 256, `label is ${boundedTargetOnly.length} characters`);
});

test("the About page renders the row under Build identity from the computed, and leaves the version row alone", () => {
  const about = sourceSlice("<h3>Build identity</h3>", "<h3>Updates</h3>");
  assert.match(
    about,
    /<dt>Installed package version<\/dt><dd><code>\{\{ installedPackageVersion \}\}<\/code><\/dd><\/div>\s*<div><dt>Installed package build<\/dt><dd><code>\{\{ installedPackageBuild \}\}<\/code><\/dd><\/div>/
  );
  assert.match(source, /installedPackageBuild\(\) \{ return installedPackageBuildLabel\(this\.snapshot\); \}/);
  // Other suites pin this line byte for byte.
  assert.ok(
    source.includes('installedPackageVersion() { return boundedText(this.snapshot && this.snapshot.package && this.snapshot.package.version, "Not reported by package API"); }')
  );
});
