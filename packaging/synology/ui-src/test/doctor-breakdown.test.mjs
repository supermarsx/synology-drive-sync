import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const source = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");

function sourceSlice(start, end) {
  const from = source.indexOf(start);
  const to = source.indexOf(end, from);
  assert.ok(from >= 0 && to > from, `missing source slice ${start} -> ${end}`);
  return source.slice(from, to);
}

async function loadDoctorHelpers() {
  const constants = sourceSlice("const DOCTOR_LEVELS", "function emptyProfileFailureRecords");
  const helpers = sourceSlice("function normalizedDoctorLevel", "function defaultSecurityPolicy");
  const moduleSource = `
    function boundedText(value, fallback = "") {
      const text = typeof value === "string" ? value : fallback;
      return String(text || fallback || "").slice(0, 65536);
    }
    function utf8ByteLength(value) {
      return Buffer.byteLength(String(value || ""), "utf8");
    }
    function boundedSanitizedTroubleshootingText(value, limit = 65536) {
      return String(value || "").slice(0, limit).trim();
    }
    function redactedTroubleshootingText(value) {
      return String(value || "").replace(/(password|token|secret)\\s*[:=]\\s*[^\\s,}]+/gi, "$1=[redacted]");
    }
    function sanitizedTroubleshootingText(value, limit = 65536) {
      return boundedSanitizedTroubleshootingText(redactedTroubleshootingText(value), limit);
    }
    const TROUBLESHOOTING_RECORD_LIMIT = 65536;
    const MAX_RESPONSE_BYTES = 1024 * 1024;
    ${constants}
    ${helpers}
    export { doctorDocumentFromResult, doctorOutputEnvelope, doctorReportFromResult, doctorTroubleshootingText, expectedDoctorSections, normalizedDoctorInventory, runningDoctorReport };
  `;
  return import(`data:text/javascript;base64,${Buffer.from(moduleSource).toString("base64")}#${Date.now()}-${Math.random()}`);
}

test("Doctor levels are explicit, standard by default, and quick never promises authentication", async () => {
  const doctor = await loadDoctorHelpers();
  assert.deepEqual(
    doctor.expectedDoctorSections("quick", false).map((section) => section.id),
    ["routing_tls", "dsm_api_discovery"]
  );
  assert.deepEqual(
    doctor.expectedDoctorSections("standard", false).map((section) => section.id),
    [
      "routing_tls", "dsm_api_discovery", "dsm_session_auth",
      "file_station_capabilities", "destination_permissions",
      "destination_inventory", "session_logout"
    ]
  );
  assert.deepEqual(
    doctor.expectedDoctorSections("extensive", true).map((section) => section.id),
    [
      "routing_tls", "dsm_api_discovery", "dsm_session_auth",
      "file_station_capabilities", "destination_permissions",
      "destination_inventory", "disposable_write_verify_cleanup", "session_logout"
    ]
  );
  assert.equal(doctor.runningDoctorReport("unsupported", false, 1).level, "standard");
  assert.match(source, /doctorForm: \{ scope: "all", level: "standard"/);
  assert.match(source, /if \(enabled\) this\.doctorForm\.level = "extensive"/);
  assert.match(source, /write_test && level !== "extensive"/);
});

test("NDJSON Doctor jobs aggregate every profile and ignore successful source Doctor records", async () => {
  const doctor = await loadDoctorHelpers();
  const entries = Array.from({ length: 7 }, (_, index) => ({
    relative_path: `folder/entry-${index + 1}`,
    name: `entry-${index + 1}`,
    kind: index % 2 ? "file" : "directory",
    size_bytes: 100 + index,
    mtime_seconds: 1788200000 + index,
    relative_path_truncated: index === 0,
    name_truncated: index === 1,
    mount_boundary: index === 2,
    unsafe_extra: "must not render"
  }));
  const first = {
    schema: "sdsync.doctor-job.v1",
    profile: "office",
    doctor: {
      schema: "sdsync.doctor.v1",
      level: "extensive",
      status: "warn",
      sections: [
        { id: "routing_tls", label: "Routing and TLS negotiation", status: "pass", detail: "TLS 1.3", elapsed_ms: 21, timing_scope: "section" },
        {
          id: "destination_inventory",
          label: "Destination inventory",
          status: "warn",
          detail: "Bounded sample",
          remote_inventory: { total_entries: 7, sample: entries, truncated: true }
        }
      ]
    }
  };
  const second = {
    schema: "sdsync.doctor-job.v1",
    profile: "archive",
    doctor: {
      schema: "sdsync.doctor.v1",
      level: "extensive",
      status: "fail",
      sections: [
        { id: "dsm_session_auth", label: "DSM session authentication", status: "fail", detail: "DSM rejected authentication" },
        { id: "session_logout", label: "Session logout", status: "skip", detail: "No session was opened" }
      ]
    }
  };
  const output = [
    JSON.stringify({ schema: "sdsync.source-doctor.v1", status: "success", sections: [{ id: "source", status: "pass" }] }),
    "plain source fallback line",
    JSON.stringify(first),
    JSON.stringify(second),
    JSON.stringify({ schema: "sdsync.doctor-job.v1", profile: "cold", status: "failed", doctor: null, error: "target setup failed" }),
    JSON.stringify({ schema: "sdsync.doctor-batch.v1", level: "extensive", status: "fail", counts: { pass: 1, warn: 1, fail: 2, skip: 1 } })
  ].join("\n");

  const report = doctor.doctorReportFromResult({ output }, false, "extensive", false, 1788200000);
  assert.equal(report.schema, "sdsync.doctor-batch.v1");
  assert.equal(report.structured, true);
  assert.deepEqual([...new Set(report.sections.map((section) => section.profile))], ["office", "archive", "cold"]);
  assert.deepEqual(report.sections.map((section) => section.state), ["ok", "warn", "failed", "skipped", "failed"]);
  assert.deepEqual(report.summary, { ok: 1, warn: 1, failed: 2, skipped: 1, pending: 0, running: 0, total: 5 });
  assert.equal(report.sections.some((section) => section.id === "source"), false);

  const inventory = report.sections.find((section) => section.id === "destination_inventory").inventory;
  assert.equal(inventory.total, 7);
  assert.equal(inventory.entries.length, 5);
  assert.equal(inventory.truncated, true);
  assert.equal(inventory.entries[0].modified, 1788200000);
  assert.equal(inventory.entries[0].relative_path_truncated, true);
  assert.equal(inventory.entries[1].name_truncated, true);
  assert.equal(inventory.entries[2].mount_boundary, true);
  assert.deepEqual(Object.keys(inventory.entries[0]).sort(), [
    "kind", "modified", "mount_boundary", "name", "name_truncated", "path",
    "relative_path_truncated", "size_bytes"
  ]);
  assert.equal(report.sections[0].timing_scope, "section");

  const copied = doctor.doctorTroubleshootingText(report, "Doctor failed", output);
  assert.match(copied, /Profile: office/);
  assert.match(copied, /Profile: archive/);
  assert.match(copied, /Profile: cold/);
  assert.match(copied, /Remote entries: 7; displayed: 5; sample truncated/);
  assert.match(copied, /Timing scope: section/);
  assert.match(copied, /relative_path_truncated=true/);
  assert.match(copied, /name_truncated=true/);
  assert.match(copied, /mount_boundary=true/);
  assert.doesNotMatch(copied, /entry-6|entry-7|unsafe_extra/);
});

test("NDJSON Doctor aggregation retains every complete profile under the response bound", async () => {
  const doctor = await loadDoctorHelpers();
  const jobs = Array.from({ length: 300 }, (_, index) => ({
    schema: "sdsync.doctor-job.v1",
    profile: `profile-${String(index).padStart(3, "0")}`,
    doctor: {
      schema: "sdsync.doctor.v1",
      level: "quick",
      status: "pass",
      sections: [{ id: "routing_tls", label: "Routing", status: "pass", detail: "Route negotiated" }]
    }
  }));
  const output = [
    ...jobs.map((job) => JSON.stringify(job)),
    JSON.stringify({ schema: "sdsync.doctor-batch.v1", level: "quick", status: "success" })
  ].join("\n");
  assert.ok(Buffer.byteLength(output, "utf8") < 1024 * 1024);

  const report = doctor.doctorReportFromResult({ output }, true, "quick", false, 1788200000);
  const profiles = [...new Set(report.sections.map((section) => section.profile).filter(Boolean))];
  assert.equal(report.output_incomplete, false);
  assert.equal(report.state, "ok");
  assert.equal(report.summary.total, 300);
  assert.equal(profiles.length, 300);
  assert.equal(profiles[0], "profile-000");
  assert.equal(profiles.at(-1), "profile-299");
});

test("an unsuccessful package outcome cannot be masked by a successful target document", async () => {
  const doctor = await loadDoctorHelpers();
  const sourceFailure = {
    schema: "sdsync.source-doctor.v1",
    status: "fail",
    sections: [{ id: "source_access", status: "fail", detail: "Local source could not be read by the package identity" }]
  };
  const successfulTarget = {
    schema: "sdsync.doctor.v1",
    level: "quick",
    status: "pass",
    sections: [
      { id: "routing_tls", status: "pass", detail: "Target route negotiated", elapsed_ms: 14, timing_scope: "section" },
      { id: "dsm_api_discovery", status: "pass", detail: "DSM API discovered", elapsed_ms: 19, timing_scope: "section" }
    ]
  };
  const output = `${JSON.stringify(sourceFailure)}\n${JSON.stringify(successfulTarget)}`;
  const report = doctor.doctorReportFromResult(
    { output, message: "Source Doctor failed before the independent target check" },
    false,
    "quick",
    false,
    1788200000
  );

  assert.equal(report.state, "failed");
  assert.equal(report.summary.failed, 1);
  const source = report.sections.find((section) => section.id === "package_source_diagnostic");
  assert.ok(source, "package-local source failure evidence must be retained");
  assert.equal(source.state, "failed");
  assert.equal(source.timing_scope, "operation");
  assert.match(source.detail, /Local source could not be read/);
  assert.equal(report.sections.find((section) => section.id === "routing_tls").state, "ok");
  const copied = doctor.doctorTroubleshootingText(report, "Doctor failed", output);
  assert.match(copied, /Package-local source diagnostic/);
  assert.match(copied, /Local source could not be read/);
  assert.doesNotMatch(copied, /"schema":"sdsync\.(?:source-)?doctor\.v1"/);
});

test("simultaneous source and target failures remain separate failed evidence", async () => {
  const doctor = await loadDoctorHelpers();
  const failedTarget = {
    schema: "sdsync.doctor-job.v1",
    profile: "office",
    status: "failed",
    doctor: {
      schema: "sdsync.doctor.v1",
      level: "standard",
      status: "fail",
      sections: [{ id: "dsm_session_auth", status: "fail", detail: "Target authentication failed" }]
    }
  };
  const sourceMarker = {
    schema: "sdsync.dsm-source-check.v1",
    status: "failed",
    exit_code: 1,
    message: "package-local source diagnostic failed"
  };
  const failedSource = {
    schema: "sdsync.source-doctor.v1",
    status: "fail",
    sections: [{ id: "source_access", status: "fail", detail: "Source is unreadable by the package identity" }]
  };
  const output = [failedTarget, sourceMarker, failedSource].map((record) => JSON.stringify(record)).join("\n");
  const report = doctor.doctorReportFromResult({ output, message: "Doctor failed" }, false, "standard", false, 1788200000);

  const target = report.sections.find((section) => section.id === "dsm_session_auth");
  const source = report.sections.find((section) => section.id === "package_source_diagnostic");
  assert.ok(target, "target failure evidence must remain visible");
  assert.equal(target.state, "failed");
  assert.match(target.detail, /Target authentication failed/);
  assert.ok(source, "source failure evidence must remain visible alongside target failure");
  assert.equal(source.state, "failed");
  assert.match(source.detail, /package-local source diagnostic failed|Source is unreadable/);
  assert.equal(report.summary.failed, 2);
  assert.equal(report.state, "failed");
  const copied = doctor.doctorTroubleshootingText(report, "Doctor failed", output);
  assert.match(copied, /Target authentication failed/);
  assert.match(copied, /Package-local source diagnostic/);
  assert.match(copied, /Source is unreadable|package-local source diagnostic failed/);
  assert.doesNotMatch(copied, /"schema":"sdsync\.(?:dsm-source-check|source-doctor|doctor-job)\.v1"/);
});

test("Doctor parsing keeps valid evidence beyond 64 KiB and rejects incomplete NDJSON as complete", async () => {
  const doctor = await loadDoctorHelpers();
  const target = {
    schema: "sdsync.doctor.v1",
    level: "quick",
    status: "pass",
    sections: [
      { id: "routing_tls", status: "pass", detail: "late target evidence", elapsed_ms: 3, timing_scope: "section" },
      { id: "dsm_api_discovery", status: "pass", detail: "late API evidence", elapsed_ms: 4, timing_scope: "section" }
    ]
  };
  const beyondLegacyLimit = `${"bounded source evidence ".repeat(3400)}\n${JSON.stringify(target)}`;
  assert.ok(Buffer.byteLength(beyondLegacyLimit, "utf8") > 64 * 1024);
  assert.ok(Buffer.byteLength(beyondLegacyLimit, "utf8") < 1024 * 1024);
  const complete = doctor.doctorReportFromResult({ output: beyondLegacyLimit }, true, "quick", false, 1);
  assert.equal(complete.structured, true);
  assert.equal(complete.state, "ok");
  assert.equal(complete.output_incomplete, false);
  assert.equal(complete.sections.find((section) => section.id === "routing_tls").detail, "late target evidence");

  const incompleteOutput = `${JSON.stringify(target)}\n{"schema":"sdsync.doctor-batch.v1","status":`;
  const incomplete = doctor.doctorReportFromResult({ output: incompleteOutput }, true, "quick", false, 1);
  assert.equal(incomplete.output_incomplete, true);
  assert.equal(incomplete.state, "failed");
  assert.equal(incomplete.sections.at(-1).id, "terminal_output_integrity");
  assert.match(incomplete.sections.at(-1).detail, /incomplete or malformed/);
  assert.match(doctor.doctorTroubleshootingText(incomplete, "Incomplete Doctor", incompleteOutput), /Terminal output complete: no/);

  const packageCapped = doctor.doctorReportFromResult({
    output: `${JSON.stringify(target)}\n${JSON.stringify({ schema: "sdsync.dsm-output-truncated.v1", truncated: true })}`
  }, true, "quick", false, 1);
  assert.equal(packageCapped.output_truncated, true);
  assert.equal(packageCapped.output_incomplete, true);
  assert.equal(packageCapped.state, "failed");
  assert.equal(packageCapped.sections.at(-1).id, "terminal_output_integrity");
  assert.match(packageCapped.sections.at(-1).detail, /package marked terminal output as truncated/);

  const overContract = doctor.doctorOutputEnvelope({ output: "x".repeat((1024 * 1024) + 1) });
  assert.equal(overContract.truncated, true);
  assert.equal(overContract.incomplete, true);
  assert.ok(Buffer.byteLength(overContract.text, "utf8") <= 1024 * 1024);
});

test("bounded plain warnings after complete NDJSON remain visible without copying raw records", async () => {
  const doctor = await loadDoctorHelpers();
  const target = {
    schema: "sdsync.doctor.v1",
    level: "quick",
    status: "pass",
    sections: [
      { id: "routing_tls", status: "pass", detail: "Target route negotiated", elapsed_ms: 3, timing_scope: "section" },
      { id: "dsm_api_discovery", status: "pass", detail: "DSM API discovered", elapsed_ms: 4, timing_scope: "section" }
    ]
  };
  const output = `${JSON.stringify(target)}\nterminal audit pending; inspect controller activity`;
  const report = doctor.doctorReportFromResult({ output }, true, "quick", false, 1);

  assert.equal(report.output_incomplete, false);
  assert.equal(report.state, "warn");
  assert.equal(report.terminal_warning, "terminal audit pending; inspect controller activity");
  const warning = report.sections.find((section) => section.id === "terminal_warning");
  assert.ok(warning);
  assert.equal(warning.state, "warn");
  assert.equal(warning.timing_scope, "transport");
  assert.match(warning.detail, /terminal audit pending/);
  const copied = doctor.doctorTroubleshootingText(report, "Doctor warning", output);
  assert.match(copied, /Terminal warning evidence/);
  assert.match(copied, /terminal audit pending/);
  assert.doesNotMatch(copied, /"schema":"sdsync\.doctor\.v1"/);
});

test("failed queued terminal output uses the one-MiB API bound rather than generic UI text", () => {
  assert.match(apiSource, /function boundedTerminalOutput\([\s\S]*?slice\(0, MAX_RESPONSE_BYTES\)/);
  assert.match(apiSource, /failure\.resultOutput = boundedTerminalOutput\(/);
  assert.doesNotMatch(apiSource, /failure\.resultOutput = boundedText\(/);
});

test("legacy output is preserved but unreported areas are skipped, never assumed healthy", async () => {
  const doctor = await loadDoctorHelpers();
  const report = doctor.doctorReportFromResult(
    { output: "legacy target rejected authentication" },
    false,
    "standard",
    false,
    1788200000
  );
  assert.equal(report.structured, false);
  assert.equal(report.state, "failed");
  assert.equal(report.summary.failed, 1);
  assert.equal(report.summary.skipped, 7);
  assert.equal(report.sections.at(-1).id, "terminal_evidence");
  assert.match(report.sections.at(-1).detail, /rejected authentication/);
});

test("Doctor surface renders section states, progress, timings, copy, and bounded inventory", () => {
  for (const marker of [
    'label="Test level"',
    'class="sdsync-doctor-progress"',
    'class="sdsync-is-spinning"',
    'aria-label="Copy Target Doctor diagnostics"',
    "doctorSummaryCards",
    "doctorStatusLabel(section.state)",
    "section.duration_ms",
    "section.timing_scope",
    "section.inventory.entries",
    "relative path ${entry.relative_path_truncated",
    "name ${entry.name_truncated",
    "mount boundary ${entry.mount_boundary",
    "bounded sample truncated",
    "doctorCleanupWarning",
    "Raw terminal evidence"
  ]) assert.ok(source.includes(marker), `missing Doctor UI marker: ${marker}`);
  assert.doesNotMatch(source, /<[^>]+v-html/);
});
