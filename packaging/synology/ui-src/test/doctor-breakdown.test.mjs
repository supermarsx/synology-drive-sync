import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const source = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");
const css = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");
const activityHelp = await readFile(new URL("../../package/ui/help/enu/activity.html", import.meta.url), "utf8");
const healthHelp = await readFile(new URL("../../package/ui/help/enu/health.html", import.meta.url), "utf8");

function sourceSlice(start, end) {
  const from = source.indexOf(start);
  const to = source.indexOf(end, from);
  assert.ok(from >= 0 && to > from, `missing source slice ${start} -> ${end}`);
  return source.slice(from, to);
}

async function loadDoctorHelpers() {
  const constants = sourceSlice("const DOCTOR_LEVELS", "function emptyProfileFailureRecords");
  const helpers = sourceSlice("function normalizedDoctorLevel", "function defaultSecurityPolicy");
  const activityEvidence = sourceSlice("function activityTroubleshootingText", "function normalizedDoctorLevel");
  const normalizedActivity = sourceSlice("function normalizedActivityEvent", "function canonicalProfileConfiguration");
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
    function troubleshootingField(value, fallback) {
      return sanitizedTroubleshootingText(String(value || fallback || ""), 128).replace(/\\s+/g, " ").trim() || fallback;
    }
    function numberOr(value, fallback = 0) {
      const number = Number(value);
      return Number.isFinite(number) ? number : fallback;
    }
    function validatedClientRequestId(value) {
      return typeof value === "string" && /^[0-9a-f]{32}$/.test(value) ? value : "";
    }
    function formatDate(value) { return String(value || "Unavailable"); }
    const TROUBLESHOOTING_RECORD_LIMIT = 65536;
    const MAX_RESPONSE_BYTES = 1024 * 1024;
    const ACTIVITY_MESSAGE_LIMIT = 2048;
    const ACTIVITY_FIELD_LIMIT = 128;
    ${constants}
    ${helpers}
    ${normalizedActivity}
    ${activityEvidence}
    export { activityTroubleshootingText, doctorDocumentFromResult, doctorInventoryRecordFromActivityMessage, doctorInventoryRecordsFromText, doctorOutputEnvelope, doctorReportFromResult, doctorTroubleshootingText, expectedDoctorSections, normalizedActivityEvent, normalizedDoctorInventory, runningDoctorReport };
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

test("Doctor copy distinguishes configured-destination and no-destination inventory branches", () => {
  const catalogCopy = sourceSlice(
    'Object.freeze({ id: "destination_permissions"',
    'Object.freeze({ id: "disposable_write_verify_cleanup"'
  );
  const tooltipCopy = sourceSlice('"doctor-level":', '"doctor-write":');
  const guidanceCopy = sourceSlice("doctorLevelGuidance()", "doctorProgressStages()");

  assert.match(tooltipCopy, /Standard and Extensive authenticate and perform bounded inventory/);
  assert.match(healthHelp, /Standard and Extensive authenticate and request one sorted, non-recursive bounded inventory page/);
  assert.match(guidanceCopy, /standard: "Authenticates[\s\S]*?performs bounded inventory/);
  assert.match(guidanceCopy, /extensive: "Deepens the same authenticated, read-only target checks[\s\S]*?same bounded inventory branch/);

  for (const [name, copy] of [
    ["section catalog", catalogCopy],
    ["level tooltip", tooltipCopy],
    ["level guidance", guidanceCopy],
    ["native Health help", healthHelp]
  ]) {
    assert.match(copy, /configured destination/i, `${name} must explain the configured-destination branch`);
    assert.match(copy, /direct child|direct-child/i, `${name} must explain direct-child sampling`);
    assert.match(copy, /skip(?:s|ped)? (?:this section|permission)/i, `${name} must explain that permission is skipped without a destination`);
    assert.match(copy, /visible shared-folder roots/i, `${name} must explain visible-share-root sampling`);
    assert.match(copy, /without selecting or traversing (?:a|any) share/i, `${name} must forbid implicit share selection or traversal`);
  }
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

test("real core-shaped visible-share and empty inventories remain section evidence", async () => {
  const doctor = await loadDoctorHelpers();
  const document = {
    schema: "sdsync.doctor.v1",
    level: "standard",
    status: "pass",
    sections: [
      {
        id: "destination_inventory",
        label: "Destination inventory",
        status: "pass",
        detail: "No visible shared folders were returned",
        elapsed_ms: 9,
        timing_scope: "section"
      }
    ],
    remote_inventory: {
      scope: "visible_shared_folders",
      root_exists: true,
      total_entries: 0,
      sample_count: 0,
      sample_limit: 5,
      truncated: false,
      truncated_count: 0,
      budget: { pages_requested: 1, traversal_depth: 0, deadline_ms: 5000 },
      sample: []
    },
    elapsed_ms: 11
  };
  const report = doctor.doctorReportFromResult({ output: JSON.stringify(document) }, true, "standard", false, 1);
  const section = report.sections.find((item) => item.id === "destination_inventory");
  assert.ok(section.inventory, "zero-entry discovery must remain visible evidence");
  assert.equal(section.inventory.scope, "visible_shared_folders");
  assert.equal(section.inventory.total, 0);
  assert.equal(section.inventory.entries.length, 0);
  assert.equal(section.inventory.truncated, false);
  const copied = doctor.doctorTroubleshootingText(report, "Visible shares", JSON.stringify(document));
  assert.match(copied, /Discovery scope: Visible shared folders/);
  assert.match(copied, /Remote entries: 0; displayed: 0/);
});

test("private Doctor inventory records parse safely in Activity and Logs at the maximum activity length", async () => {
  const doctor = await loadDoctorHelpers();
  const sample = Array.from({ length: 5 }, (_, index) => ({
    path: `/${String(index + 1).padStart(2, "0")}-${"p".repeat(230)}`,
    name: `entry-${index + 1}-${"n".repeat(70)}`,
    kind: index % 2 ? "file" : "folder",
    username: "must-not-render-user",
    password: "must-not-render-password",
    session: "must-not-render-session",
    url: "https://must-not-render.invalid",
    acl: "must-not-render-acl",
    digest: "must-not-render-digest",
    server_detail: "must-not-render-server"
  }));
  const record = {
    schema: "sdsync.dsm-doctor-inventory.v1",
    epoch: 1788200000,
    level: "info",
    category: "operations",
    event: "doctor_inventory",
    action: "doctor",
    profile: "office",
    scope: "direct_children",
    total_entries: 7,
    truncated: true,
    sample_count: 5,
    sample,
    username: "outer-user-must-not-render",
    session: "outer-session-must-not-render"
  };
  const message = `Doctor inventory evidence ${JSON.stringify(record)}`;
  assert.ok(message.length > 2048, "fixture must exceed the ordinary Activity display bound");
  assert.ok(message.length < 4096, "fixture must remain inside the package Activity record bound");

  const event = doctor.normalizedActivityEvent({
    epoch: 1788200000,
    code: "doctor.inventory",
    profile: "office",
    state: "succeeded",
    category: "operations",
    level: "info",
    message
  });
  assert.equal(event.message.length, 2048, "ordinary message rendering remains bounded");
  assert.ok(event.doctor_inventory, "structured evidence must be parsed before display truncation");
  assert.equal(event.doctor_inventory.inventory.entries.length, 5);
  assert.equal(event.doctor_inventory.inventory.total, 7);
  assert.equal(event.doctor_inventory.inventory.scope, "direct_children");
  assert.deepEqual(Object.keys(event.doctor_inventory.inventory.entries[0]).sort(), [
    "kind", "modified", "mount_boundary", "name", "name_truncated", "path",
    "relative_path_truncated", "size_bytes"
  ]);
  const copied = doctor.activityTroubleshootingText(event);
  assert.match(copied, /Doctor discovery scope: Direct children/);
  assert.match(copied, /Doctor discovery entries: 7; displayed: 5; sample truncated/);
  assert.doesNotMatch(copied, /must-not-render-(?:user|password|session|acl|digest|server)|must-not-render\.invalid/);

  const logRecords = doctor.doctorInventoryRecordsFromText([
    JSON.stringify({ schema: "unrelated.log.v1", password: "ignore-me" }),
    JSON.stringify(record)
  ].join("\n"));
  assert.equal(logRecords.length, 1);
  assert.equal(logRecords[0].profile, "office");
  assert.equal(logRecords[0].inventory.entries.length, 5);

  const emptyRecord = Object.assign({}, record, {
    scope: "visible_shared_folders",
    total_entries: 0,
    truncated: false,
    sample_count: 0,
    sample: []
  });
  const parsedEmpty = doctor.doctorInventoryRecordsFromText(JSON.stringify(emptyRecord));
  assert.equal(parsedEmpty.length, 1);
  assert.equal(parsedEmpty[0].inventory.scope, "visible_shared_folders");
  assert.equal(parsedEmpty[0].inventory.total, 0);
  assert.deepEqual(parsedEmpty[0].inventory.entries, []);
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

test("private discovery cards are dark, responsive, overflow-contained, and covered by DSM Help", () => {
  for (const marker of [
    "sdsync-inventory-evidence",
    "sdsync-inventory-evidence-summary",
    "sdsync-inventory-evidence-entry",
    "sdsync-log-inventory-evidence",
    "sdsync-doctor-inventory-empty",
    '["doctor", "Doctor discovery"]'
  ]) assert.ok(source.includes(marker) || css.includes(marker), `missing private discovery UI marker: ${marker}`);
  assert.match(css, /\.sdsync-inventory-evidence\s*\{[^}]*min-width:\s*0[^}]*background:\s*var\(--sdsync-control\)/s);
  assert.match(css, /\.sdsync-inventory-evidence-entry\s*\{[^}]*grid-template-columns:\s*48px minmax\(0, 1fr\) minmax\(72px, 0\.35fr\)[^}]*min-width:\s*0/s);
  assert.match(css, /\.sdsync-inventory-evidence-entry > code,[\s\S]*?min-width:\s*0[\s\S]*?overflow:\s*hidden[\s\S]*?text-overflow:\s*ellipsis[\s\S]*?white-space:\s*nowrap/);
  assert.match(css, /\.sdsync-app\.sdsync-compact-shell \.sdsync-inventory-evidence-entry\s*\{[^}]*grid-template-columns:\s*44px minmax\(0, 1fr\)/s);
  assert.match(activityHelp, /Doctor discovery contains private, package-local structure evidence/);
  assert.match(activityHelp, /at most five logical path\/name and folder\/file entries/);
  assert.match(activityHelp, /not sent to a profile's generic remote log collector/);
});
