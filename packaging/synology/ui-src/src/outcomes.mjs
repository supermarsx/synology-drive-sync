/**
 * Pure derivation of what the dashboard says about run outcomes.
 *
 * The package bridge attaches an `outcomes` block (schema
 * `sdsync.dsm-outcomes.v1`) to every snapshot. Nothing may fail silently, so
 * this module turns that block into copy that names the cause and the next
 * step, and every value it does not recognise is shown as `unknown (<token>)`
 * instead of being hidden or thrown on. Imports only the dependency-free
 * textSafety.mjs, so the widget, the AppWindow and the tests all read the same
 * rules.
 */

import { decodePercentPath, plainText, visibleText } from "./textSafety.mjs";

const OUTCOME_LABELS = Object.freeze({
  succeeded: "Succeeded", partial: "Partial", failed: "Failed", not_run: "Not run",
  busy: "Busy", cancelled: "Cancelled", stale: "Stale", running: "Running", unknown: "Status unknown"
});
const OUTCOME_LEVELS = Object.freeze({
  succeeded: "ok", partial: "warn", failed: "fail", not_run: "idle",
  busy: "warn", cancelled: "warn", stale: "fail", running: "warn", unknown: "fail"
});
const CACHE_STATES = ["trusted", "last_run_failed", "last_run_partial", "no_run_recorded"];

// [cause, next step]. Keyed by the reason code before any `:suffix`.
const REASONS = Object.freeze({
  preflight_config_invalid: ["the profile configuration did not validate", "Open Profiles and correct the highlighted setting."],
  preflight_log_file_path: ["package paths mismatch (the profile log file is not the package-managed path)", "Repair the package, or reset the profile's log file setting."],
  preflight_policy: ["the security policy refused the profile", "Review Security: plain HTTP, unverified TLS or destructive options are refused."],
  preflight_unsafe_state: ["a package file failed its ownership or mode check", "Repair or reinstall the package."],
  no_password: ["no password is stored", "Enter the password on the Profiles page."],
  // Every token below is written by sdsync-run, sdsync-controller or the bridge
  // (test/reason-vocabulary.test.mjs reads those writers and fails on a token
  // with no row here).
  core_failed: ["the sync core exited with an error", "Review Logs for the cause, then run again."],
  core_oom_killed: ["the sync core was killed by the out-of-memory killer: the source holds more entries than this NAS has memory for", "Split the profile into smaller ones (a scope or subfolder profiles), then run again."],
  core_killed: ["the sync core was killed (SIGKILL) without reporting an error, possibly out of memory", "Check the DSM system log for the out-of-memory killer; split the profile if the source is very large."],
  profiles_skipped: ["some profiles were not run because they failed their pre-run checks", "Fix the profiles listed above, then run again."],
  runner_error: ["the package runner stopped before the core started, without naming a cause", "Review Logs, then run again."],
  audit_unavailable: ["the mandatory audit record could not be written", "Check free space on the volume, then repair or reinstall the package."],
  binary_missing: ["the installed sync program is missing or not executable", "Repair or reinstall the package."],
  config_missing: ["no readable package configuration was found", "Create a profile on the Profiles page, or repair the package."],
  policy_unreadable: ["the security policy file is not a readable private document", "Repair or reinstall the package."],
  policy_operational: ["the security policy blocks Doctor, plan and run actions", "Allow operational actions on the Security page."],
  admission_failed: ["the package could not confirm it was safe to start a run", "Review Logs and run again; repair the package if it repeats."],
  no_profiles: ["no profiles are configured", "Create a profile on the Profiles page."],
  state_unwritable: ["a package state file could not be written (when a run was being started, the run was not started because its record could not be written)", "Check free space on the package volume and file ownership, or repair the package."],
  run_stalled: ["the run made no progress for 30 minutes and was stopped", "Review Logs; check the source and target are reachable, then run again."],
  launch_failed: ["the sync core could not be started", "Review Logs for the cause, then run again; repair the package if it repeats."],
  consumer_timeout: ["the dashboard request did not finish within its deadline, so its outcome is unknown", "Try again; review Logs if it repeats."],
  consumer_unwaitable: ["the dashboard request's worker was started but could not be waited for, so how it ended is unknown", "Check the current state before trying again; review Logs if it repeats."],
  control_dir_unreadable: ["the service cannot read the dashboard's request queue, so requests are not seen", "Review Logs for control_dir_unreadable; repair the package if it repeats."],
  lasting_state_unreadable: ["the record that holds this profile's automatic runs could not be read, so automatic runs stay held", "Click Run now to clear it; repair the package if it repeats."],
  identity_failed: ["the runner could not record its own identity before the core started", "Review Logs and run again; repair the package if it repeats."],
  profile_unreadable: ["the profile file could not be read", "Save the profile again on the Profiles page, or repair the package."],
  profile_invalid: ["the profile did not validate", "Open Profiles and correct the highlighted setting."],
  policy_destructive: ["the security policy forbids destructive sync (deleting on the target)", "Allow destructive sync on the Security page, or turn deletions off in the profile."],
  policy_http: ["the security policy forbids plain HTTP targets", "Use HTTPS, or allow HTTP targets on the Security page."],
  policy_invalid_tls: ["the security policy forbids skipping TLS certificate verification", "Install a valid certificate, or allow it on the Security page."],
  policy_remote_logging: ["the security policy forbids remote logging", "Turn remote logging off in the profile, or allow it on the Security page."],
  policy_empty_source: ["the security policy forbids syncing an empty source", "Check the source folder is mounted, or allow empty sources on the Security page."],
  password_missing: ["no password is stored for the profile", "Enter the password on the Profiles page."],
  secret_unsafe: ["a stored secret file failed its ownership or mode check", "Enter the password again on the Profiles page, or repair the package."],
  log_file_unsafe: ["the profile's log file path failed its ownership or mode check", "Repair the package, or reset the profile's log file setting."],
  log_path_mismatch: ["package paths mismatch (the profile log file is not the package-managed path)", "Repair the package, or reset the profile's log file setting."],
  busy: ["another run, Doctor or management action holds the package, or the package is stopping or upgrading", "A routine tries again on its own; a run you started is not repeated, so run it again if you still need it."],
  interrupted: ["the routine was interrupted before it finished", "It is retried automatically."],
  admission_refused: ["the package refused to start the run (lifecycle change or lock)", "It is retried automatically."],
  dependency: ["a routine this one depends on has not succeeded", "Fix the dependency routine first."],
  source: ["the routine's source folder is missing or unreadable", "Check the source folder is mounted and readable."],
  window: ["the routine is outside its daily window", "It runs when the window opens."],
  pid_unreadable: ["controller.pid exists but holds no process ID", "Stop the package in Package Center, then Run it again."],
  identity_refused: ["the process the PID file names is not the verified controller", "Stop the package in Package Center, then Run it again."],
  identity_changed: ["the controller's identity changed while the package was telling it to reload", "Stop the package in Package Center, then Run it again."],
  signal_failed: ["the package could not signal the running controller", "Stop the package in Package Center, then Run it again."],
  lock_busy: ["another run, Doctor or management action held the run lock", "A routine tries again on its own; a run you started is not repeated, so run it again if you still need it."],
  runner_died: ["the runner died before it finished (killed, out of memory or power loss)", "Review Logs, then run again."],
  core_partial: ["the run changed the destination and then failed part-way", "Review Logs, then run again; nothing is rolled back."],
  runner_vanished: ["the runner died before it finished (killed, out of memory or power loss)", "Review Logs, then run again."],
  supervisor_refused: ["the package supervisor refused the sync command", "Review Logs and the package version."],
  dependency_unmet: ["a routine this one depends on has not succeeded", "Fix the dependency routine first."],
  outside_window: ["the routine is outside its daily window", "It runs when the window opens."],
  cancelled: ["the run was stopped", "A routine runs again on its own; a run you started is not repeated, so run it again when ready."],
  batch_target_failed: ["the run stopped at this profile, so later profiles did not run", "Fix this profile, then run again."],
  not_reached: ["an earlier profile failed, so this one did not run", "Fix the failing profile first."],
  authentication: ["the target rejected the login (password changed, a one-time code or 2-step verification is required, or the account is locked or disabled)", "Check the account on the target DSM, then re-enter the password and test the connection."],
  network: ["the target could not be reached", "Check the network and target address."],
  http_status: ["the target answered with an HTTP error", "Review Logs for the status."],
  timeout: ["the target did not answer in time", "Check target load and network, then retry."],
  remote_response_invalid: ["the target sent an unreadable answer", "Review Logs; check the target is File Station."],
  dsm_api_error: ["the target DSM API returned an error", "Review Logs for the API error."],
  dsm_api_unsupported: ["the target DSM does not support a required API", "Update the target DSM."],
  share_not_writable: ["the destination share is not writable", "Grant the sync account write access."],
  remote_path_refused: ["the destination path was refused", "Choose a different destination path."],
  type_conflict: ["a file and folder share one name", "Rename one of them."],
  protected_conflict: ["a protected destination file would be overwritten", "Resolve it manually."],
  empty_source_guard: ["the source is empty, so nothing was synced", "Check the source folder is mounted."],
  delete_limit: ["too many deletions were planned", "Review the plan before confirming."],
  source_invalid: ["the local source is invalid", "Check the source folder exists and is readable."],
  local_entry_unsupported: ["an unsupported local entry blocked the run", "Remove or exclude it."],
  local_io: ["reading local files failed", "Check source permissions and disk health."],
  source_changed: ["the source changed during the run", "It is picked up on the next run."],
  resync_ticket_stale: ["the resync confirmation expired", "Start the resync again."],
  content_verification_failed: ["uploaded content did not verify", "Run again; check target storage."],
  remote_changed: ["the destination changed during the run", "Run again."],
  server_copy_unavailable: ["server-side copy was unavailable", "Files re-upload on the next run."],
  server_copy_failed: ["server-side copy failed", "Files re-upload on the next run; review Logs if it repeats."],
  reconciliation_pending: ["a previous change is still being reconciled", "Wait for it to settle, then run again."],
  credential_unavailable: ["the stored credential could not be read", "Re-enter the password."],
  config_invalid: ["the configuration is invalid, or two profiles have overlapping destinations and one of them deletes (the detail names the pair)", "Correct it on the Profiles page; profiles whose destinations overlap need separate destinations."],
  https_required: ["the target must use HTTPS", "Change the target to HTTPS."],
  scope_not_found: ["the named profile does not exist", "Check the profile name."],
  unclassified: ["the failure has no specific classification", "Review Logs."],
  changed_during_run: ["files changed during the run", "They sync next run."],
  delete_bound: ["deletions were bounded", "The rest is deleted next run."],
  server_copy_fallback: ["server-side copy was refused", "Files re-upload on the next run."],
  excluded: ["files were excluded", "Check the exclude rules if unexpected."],
  upload_failed: ["some uploads failed", "They retry on the next run."],
  evidence_canary_mismatch: ["a stored or carried checksum disagreed with a live one, so the run re-checked every file live", "Nothing to do for this run; if it repeats, review Logs and report it."],
  pair_index_unusable: ["the verified-checksum index could not be used, so every file was checked live (slower, still correct)", "Review Logs for the cause; the index is rebuilt on its own."],
  pair_index_store_failed: ["the verified-checksum index could not be saved, so the next run checks files live again", "Check free space on the volume holding the package."],
  tree_too_large_for_memory: ["the source holds more entries than this run has memory for, so nothing was synced", "Split the profile into smaller ones (a scope or subfolder profiles)."],
  "tree-too-large-for-memory": ["not synced until you fix this: the source holds more entries than this run has memory for, so nothing was synced", "Split the profile into smaller ones (a scope or subfolder profiles)."],
  "source-changed": ["files changed during the run", "They sync next run."],
  "source-vanished": ["a source file disappeared during the run", "Nothing to do unless it was unintended."],
  "source-unreadable": ["a source file could not be read", "Check its permissions."],
  "unsupported-local-entry": ["an unsupported local entry was left out", "Remove or exclude it."],
  "remote-digest-unavailable": ["the target gave no digest to verify against", "It is re-checked on the next run."],
  "remote-changed": ["the destination changed during the run", "Run again."],
  "unrepresentable-name": ["a file name cannot be represented on the target", "Rename it."],
  "remote-unreadable": ["the NAS does not let this account read this file or folder, or list a folder's contents, so it is never compared or deleted", "Grant the sync account read permission on it."],
  deferred_paths: ["some paths were deferred", "They sync on the next run; review the entry.deferred records in the sync log."],
  "listing-incomplete": ["the destination listed fewer or more entries than expected, so deletions in those folders are withheld", "Run again; review Logs if it repeats."],
  "remote-refused": ["not synced until you fix this: the NAS refuses this name, or this account's access to it", "Rename it, or fix the sync account's permissions on it in File Station. It is named on every run until then."],
  "delete-not-permitted": ["not synced until you fix this: the NAS does not let this account delete it, so it is kept", "Grant the sync account delete permission on it in File Station, or remove it there."],
  "local-permission-denied": ["not synced until you fix this: the sync account may not read it", "Grant the sync account read permission on it. It is named on every run until then."],
  "case-rename-pending": ["not synced until you fix this: a case-only rename on the NAS cannot finish (the old spelling is still there, or the renamed folder holds @eaDir or excluded items)", "Rename the folder or remove the old spelling in File Station."],
  "remote-skipped": ["not synced until you fix this: a NAS name or link this tool will not follow was left alone", "It is never deleted or overwritten and is named on every run; rename or remove it in File Station."],
  "unsupported-entry": ["not synced until you fix this: a name, path or file type Synology Drive cannot hold", "Rename it, or exclude it. It is named on every run until then."],
  "type-conflict": ["not synced until you fix this: a file replaced a folder, the reverse, or a local folder sits where the NAS has a mount point", "Run with deletion enabled, or remove the excluded or DSM-managed items inside the NAS folder."],
  "remote-link": ["not synced until you fix this: a local folder sits where the NAS has a link to somewhere else, and nothing is synced there", "Replace the link with a folder on the NAS."],
  remote_refused_share_wide: ["the NAS refused most folders or files for one cause (this account's permission or a share-wide restriction), so the run stopped instead of working through them one by one", "Fix the sync account's permission on the share in File Station, run Doctor, then run again."],
  remote_real_path_unusable: ["DSM reported real paths in an unexpected shape, so almost every folder looked like a link; nothing was synced", "Update the package; if it repeats, report it with the DSM version."],
  "remote-real-path-unusable": ["DSM reported real paths in an unexpected shape, so almost every folder looked like a link; nothing was synced", "Update the package; if it repeats, report it with the DSM version."],
  "protected-descendant": ["not synced until you fix this: a folder deleted locally is kept on the NAS because it still holds an excluded or DSM-managed item", "Remove that item in File Station."],
  deferred: ["a path was deferred", "It syncs on the next run; the entry.deferred record names the cause."],
  "server-copy-failed": ["server-side copy failed", "Files re-upload on the next run."],
  "delete-failed": ["a deletion failed", "It is retried on the next run."],
  "windows-attributes": ["files marked SYSTEM or TEMPORARY were skipped", "Clear the attribute or exclude them."],
  "dehydrated-placeholder": ["an online-only cloud file (for example OneDrive) or an offline file was skipped, because reading it would trigger a download", "Make it available offline (Always keep on this device) to sync it."],
  "link-or-reparse-point": ["links and reparse points were skipped", "They are never followed."],
  "local-case-collision": ["names differing only in case collide locally", "Rename one."],
  "remote-case-collision": ["names differing only in case collide on the target", "Rename one."],
  "case-rename-needs-delete": ["a case-only rename needs a deletion", "Enable bounded deletion or rename back."]
});

// Record-error codes the bridge can name, with what they mean for the reader.
const RECORD_ERROR_COPY = Object.freeze({
  clock_skew: "a run record is stamped in the future by the NAS clock, so hang detection is switched off for it; check the NAS time settings"
});

// The verified-pair (checksum) index a profile's last run used. States are the closed list the
// package writes; the issue codes below are the warning-level ones it names, in plain words.
const INDEX_STATES = Object.freeze(["off", "cold", "warm", "refreshed", "unusable", "local-only"]);
const INDEX_INFO_ISSUES = Object.freeze(["pair-index-retired", "pair-index-not-applicable", "pair-index-unsupported-platform", "pair-index-plan-clock-unbounded", "pair-index-compaction-failed", "pair-index-link-guard-unchecked"]);
const INDEX_INFO_NOTES = Object.freeze({
  "pair-index-retired": "rebuilt, it belonged to another source or remote",
  "pair-index-not-applicable": "not used for this profile",
  "pair-index-unsupported-platform": "not supported on this platform",
  "pair-index-plan-clock-unbounded": "some files were checked live again because the destination's clock is more than 20 seconds from this NAS's; check the time settings",
  "pair-index-compaction-failed": "the index file could not be compacted this run and stays larger than it needs to be; it still works",
  "pair-index-link-guard-unchecked": "stored checksums were not used this run because the destination listed some folders without a real path, so every file was checked live"
});
const INDEX_ISSUE_COPY =Object.freeze({
  "pair-index-corrupt": "the checksum index was damaged; it was set aside and rebuilt from this run, so every file was checked live",
  "pair-index-locked": "another process was using the checksum index, so this run did not use it and every file was checked live",
  "pair-index-readonly": "the checksum index is on a read-only location, so it could not be used or updated and every file was checked live",
  "pair-index-store-failed": "the checksum index could not be saved, so the next run checks files live again",
  "pair-index-unsafe": "the checksum index (or the record of an earlier crash beside it) could not be examined or failed its safety check (a link, or the wrong owner or mode), so it was not used and every file was checked live",
  "pair-index-directory-unusable": "the folder that holds the checksum index could not be used, so every file was checked live",
  "pair-index-unbound": "the checksum index belongs to a different source or destination, so it was not used and every file was checked live",
  "pair-index-format-unsupported": "the checksum index was written in a format or version this package does not read, so it was not used and every file was checked live",
  "pair-index-canary-mismatch": "a stored checksum disagreed with a live one, so the index was not trusted and every file was checked live",
  "pair-index-writeback-unknown": "this NAS's writeback setting could not be read, so files changed in the last minute are treated as unsettled and checked live",
  "pair-index-writeback-unbounded": "periodic writeback is switched off on this NAS (vm.dirty_writeback_centisecs is 0), so the checksum index cannot tell when a changed file has settled and every file was checked live",
  "pair-index-nas-clock-skewed": "the destination's clock (or a proxy's in front of it) is more than 20 seconds from this NAS's, so the files it reported were checked live again; check the time settings",
  "pair-index-cleanup-failed": "a set-aside copy of the checksum index, or the record of a crash in it, could not be cleaned up, so it may stay and use disk space; the path and the reason are in the detail",
  "pair-index-remote-ctime-static": "the destination did not show this run's write in a file's change time, so the destination's checksums are always computed live and only the local read is skipped"
});

const SAFE_TOKEN = /^[A-Za-z0-9_:.-]{1,64}$/;
const has = (table, key) => Object.prototype.hasOwnProperty.call(table, key);

function obj(value) { return value && typeof value === "object" && !Array.isArray(value) ? value : {}; }
function count(value) { const n = Number(value); return Number.isFinite(n) && n >= 0 ? Math.floor(n) : null; }
function token(value) { return typeof value === "string" && value ? value : ""; }
function shown(value) { return SAFE_TOKEN.test(value) ? value : "unreadable"; }

/** `unknown (<token>)` for anything outside a closed vocabulary; never throws. */
export function unknownLabel(value) { return `unknown (${shown(token(value))})`; }

export function outcomeLabel(value) {
  const key = token(value);
  return Object.prototype.hasOwnProperty.call(OUTCOME_LABELS, key) ? OUTCOME_LABELS[key] : unknownLabel(key);
}

export function outcomeLevel(value) {
  const key = token(value);
  return Object.prototype.hasOwnProperty.call(OUTCOME_LEVELS, key) ? OUTCOME_LEVELS[key] : "unknown";
}

// A count the package may leave out (null), which is not the same as zero.
function countOrNull(value) { return value === null || value === undefined ? null : count(value); }
function grouped(n) { return String(n).replace(/\B(?=(\d{3})+(?!\d))/g, ","); }
function plural(n, one, many) { return `${grouped(n)} ${n === 1 ? one : many}`; }

/** The `index` member of a profile outcome: null when the run used none, else a flat record. */
function normalizeIndex(raw) {
  if (raw === undefined || raw === null) return null;
  const empty = { state: "", entries: null, hits: null, misses: null, canaryChecked: null, canaryMismatches: null, issue: "", issueDetail: "" };
  if (typeof raw !== "object" || Array.isArray(raw)) return { ...empty, state: "unreadable", malformed: true };
  return {
    state: token(raw.state),
    entries: countOrNull(raw.entries),
    hits: countOrNull(raw.hits),
    misses: countOrNull(raw.misses),
    canaryChecked: countOrNull(raw.canary_checked),
    canaryMismatches: countOrNull(raw.canary_mismatches),
    issue: token(raw.issue),
    issueDetail: typeof raw.issue_detail === "string" ? plainText(raw.issue_detail, 160) : "",
    malformed: false
  };
}

/**
 * "Checksum index: warm, 1,000 pairs; 975 reused, 25 checked live (25 canary)": what the
 * profile's last run did with the verified-pair index, or "" when it used none.
 */
export function indexLine(index) {
  if (!index) return "";
  if (index.malformed) return "Checksum index: unreadable (the package sent a value this window cannot read)";
  const state = INDEX_STATES.includes(index.state) ? index.state : unknownLabel(index.state);
  let line = `Checksum index: ${state}`;
  if (index.entries !== null) line += `, ${plural(index.entries, "pair", "pairs")}`;
  if (index.state === "off") {
    line += "; every file was checked live";
  } else {
    if (index.hits !== null && index.misses !== null) {
      line += `; ${index.hits} reused, ${index.misses} checked live`;
      if (index.canaryChecked > 0) line += ` (${index.canaryChecked} canary)`;
    }
    if (index.canaryMismatches > 0) line += `, ${plural(index.canaryMismatches, "canary mismatch", "canary mismatches")}`;
  }
  // Info-level notes explain a state (a slow first run after a profile is re-created); they are
  // not problems, so they ride on the line and never raise an attention item.
  const note = has(INDEX_INFO_NOTES, index.issue) ? INDEX_INFO_NOTES[index.issue] : "";
  if (note) line += `; ${note}${index.issueDetail ? ` (${index.issueDetail})` : ""}`;
  return line;
}

/** The attention item for one profile's index, or null when it needs none. */
function indexAttentionItem(profile) {
  const index = profile.index;
  if (!index) return null;
  const name = visibleText(profile.profile);
  const id = `pair-index-${profile.profile}`;
  if (index.malformed) {
    return { id, level: "warn", title: `${name}: checksum index unreadable`, detail: "The package sent an index record this window cannot read, so whether the checksum index was used is unknown. Every file is still checked; review Logs if it repeats." };
  }
  if (index.state && !INDEX_STATES.includes(index.state)) {
    return { id, level: "warn", title: `${name}: checksum index state not recognised`, detail: `The checksum index reported ${unknownLabel(index.state)}. Review Logs.` };
  }
  if (!index.issue || INDEX_INFO_ISSUES.includes(index.issue)) return null;
  const known = has(INDEX_ISSUE_COPY, index.issue);
  const cause = known
    ? `${INDEX_ISSUE_COPY[index.issue].charAt(0).toUpperCase()}${INDEX_ISSUE_COPY[index.issue].slice(1)}`
    : `The checksum index reported an unknown index issue (${shown(index.issue)})`;
  return {
    id, level: "warn", title: `${name}: checksum index needs attention`,
    detail: `${cause}${index.issueDetail ? `. ${index.issueDetail}` : ""}.`.replace(/\.+$/, ".")
  };
}

/** The `lasting` list: profiles held after a user-fixable failure. Every entry is kept. */
function normalizeLasting(raw) {
  return (Array.isArray(raw) ? raw : []).slice(0, 256).map((item) => {
    const l = obj(item);
    return {
      profile: token(l.profile),
      reason: token(l.reason),
      detail: typeof l.detail === "string" ? plainText(l.detail, 240) : "",
      sinceEpoch: countOrNull(l.since_epoch),
      error: typeof l.error === "string" ? plainText(l.error, 240) : ""
    };
  });
}

function lastingAttentionItem(held) {
  const name = held.profile ? visibleText(held.profile) : "A profile";
  let detail;
  if (held.reason === "lasting_state_unreadable") {
    detail = `The record that holds this profile's automatic runs could not be read${held.error ? ` (${held.error})` : ""}; automatic runs stay held. Click Run now to clear it.`;
  } else if (held.reason === "core_oom_killed") {
    detail = `${reasonCopy("core_oom_killed").text} ${held.detail}${held.detail ? " " : ""}It runs again when you click Run now, or when you change this profile's settings or the performance level.`;
  } else {
    detail = `This profile is held for ${unknownLabel(held.reason)}${held.detail ? `: ${held.detail}` : ""}. Click Run now to try again; review Logs.`;
  }
  return { id: `lasting-${held.profile || "unnamed"}`, level: "warn", title: `${name}: not synced until you fix this`, detail };
}

/** `outcomes.engine`: which service engine runs, from the package's engine record. */
function normalizeEngine(raw) {
  if (raw === undefined) return null;
  const e = obj(raw);
  return {
    active: token(e.active),
    file: token(e.file),
    detail: typeof e.detail === "string" ? plainText(e.detail, 240) : ""
  };
}

/** "daemon" or "shell (fallback)" for the About page; "" when the package reports no engine. */
export function engineLabel(engine) {
  if (!engine) return "";
  if (engine.active === "daemon") return "daemon";
  if (engine.active === "shell") return "shell (fallback)";
  return unknownLabel(engine.active);
}

function engineAttentionItem(engine) {
  if (!engine) return null;
  if (engine.file === "invalid") {
    return { id: "engine-invalid", level: "warn", title: "The service engine setting is not understood; the daemon runs", detail: `${engine.detail || "The engine record could not be read."} Correct or remove it, then stop the package in Package Center and Run it again.` };
  }
  if (!["absent", "daemon", "shell"].includes(engine.file)) {
    return { id: "engine-unknown", level: "warn", title: "The service engine setting is not recognised", detail: `The package reported the engine setting as ${unknownLabel(engine.file)}. Review Logs.` };
  }
  return null;
}

/**
 * What a routine's `defer_reason` means. Most are the same facts the run-level
 * reasons name, but `delete_bound` differs: for the core it says a run's
 * deletions were bounded and the rest follows next run; for a routine it says the
 * routine's own deletion limit is weaker than its profile's max-delete, so the
 * controller refuses every run before it starts and the routine can never run
 * until someone changes a limit. { text, level, blocked }.
 */
export function routineDeferCopy(reason) {
  const code = token(reason);
  if (code === "delete_bound") {
    return {
      text: "this routine can never run: its deletion limit is lower than the profile's max-delete, so every run is refused before it starts. Raise the routine's deletion limit, or lower the profile's max-delete, on the Routines page.",
      level: "fail",
      blocked: true
    };
  }
  return { text: has(REASONS, code) ? reasonCopy(code).text : `reason ${unknownLabel(code)}`, level: "warn", blocked: false };
}

/** { text, known } for a reason code; text names the cause and the next step. */
export function reasonCopy(value) {
  const code = token(value);
  if (!code) return { text: "", known: true };
  const [base, suffix] = [code.split(":")[0], code.includes(":") ? code.slice(code.indexOf(":") + 1) : ""];
  if (!Object.prototype.hasOwnProperty.call(REASONS, base) || !SAFE_TOKEN.test(code)) {
    return { text: `unknown reason (${shown(code)})`, known: false };
  }
  const [cause, next] = REASONS[base];
  const subject = suffix ? ` (${suffix})` : "";
  return { text: `${cause}${subject}. ${next}`, known: true };
}

function factsOf(record) {
  const r = obj(record);
  const rec = r.recovered && typeof r.recovered === "object" && !Array.isArray(r.recovered) ? r.recovered : null;
  return {
    incompleteListings: count(r.incomplete_listings),
    folders: (Array.isArray(r.incomplete_listing_folders) ? r.incomplete_listing_folders : [])
      .filter((f) => typeof f === "string" && f).map((f) => f.replace(/[^\x20-\x7e]/g, "?").slice(0, 120)).slice(0, 5),
    mtimeNotKept: count(r.mtime_not_kept),
    // NAS folders listed, or passed on the scope chain, with no real path to check entries against.
    linkGuardUnchecked: count(r.link_guard_unchecked),
    recovered: rec ? {
      reauthenticationFailures: count(rec.reauthentication_failures),
      serverTasksNotStopped: count(rec.server_tasks_not_stopped),
      keepaliveFailures: count(rec.keepalive_failures)
    } : null
  };
}

function tokens(value) {
  return (Array.isArray(value) ? value : []).filter((item) => typeof item === "string" && item).map((item) => shown(item)).slice(0, 8);
}

/** Text for one warning token; unknown tokens are shown raw. `r` is a normalised run or profile. */
export function warningText(warning, r = {}) {
  const n = (value, one, many) => `${value} ${value === 1 ? one : many}`;
  const rec = r.recovered || {};
  if (warning === "skipped_entries") return `${r.skipped > 0 ? n(r.skipped, "entry", "entries") : "Some entries"} skipped and will not sync until fixed`;
  if (warning === "deferred_entries") return `${r.deferred > 0 ? n(r.deferred, "entry", "entries") : "Some entries"} could not be synced and will be left out until fixed`;
  if (warning === "incomplete_listings") {
    const names = (r.folders || []).map((f) => (f === "." ? "the destination root" : f));
    const more = r.incompleteListings > names.length && names.length ? ", ..." : "";
    return `Listing incomplete in ${r.incompleteListings > 0 ? n(r.incompleteListings, "folder", "folders") : "some folders"}${names.length ? ` (${names.join(", ")}${more})` : ""}: deletes there were held back; run again, and review Logs if it repeats`;
  }
  if (warning === "mtime_not_kept") return `${r.mtimeNotKept > 0 ? n(r.mtimeNotKept, "upload", "uploads") : "Some uploads"} lost their modification time; the next run will upload them again. Run Doctor's write probe`;
  if (warning === "reauthentication_failures") return `${rec.reauthenticationFailures > 0 ? n(rec.reauthenticationFailures, "re-login failure", "re-login failures") : "Re-login failures"} during the run; check the account and password on the target DSM`;
  if (warning === "server_tasks_not_stopped") return `${rec.serverTasksNotStopped > 0 ? n(rec.serverTasksNotStopped, "server-side task", "server-side tasks") : "Server-side tasks"} could not be stopped on the target; check its Task Manager`;
  if (warning === "keepalive_failures") return `Recovered from ${rec.keepaliveFailures > 0 ? n(rec.keepaliveFailures, "keep-alive failure", "keep-alive failures") : "keep-alive failures"}; check the network if it repeats`;
  if (warning === "link_guard_unchecked") return `${r.linkGuardUnchecked > 0 ? `Link check unavailable for ${n(r.linkGuardUnchecked, "folder", "folders")}` : "Link check unavailable for some folders"}: DSM gave no real path to check entries against, so links there could not be ruled out; nothing was refused. Run Doctor`;
  if (warning === "stall_unobservable") return "Cannot tell whether this run is stuck: the core's I/O counters are not readable here, so its progress cannot be observed and a stuck run would not be flagged";
  if (warning === "stalled") return "Run may be stuck: no progress for over 30 minutes";
  if (warning === "long_running") return `Running for ${r.runningFor !== null && r.runningFor !== undefined ? spanLabel(r.runningFor) : "a long time"}; check whether it is stuck`;
  return `unknown warning (${shown(warning)})`;
}

/** Joined warning copy for a normalised run or profile, or "". */
export function warningsSentence(record) {
  const r = obj(record);
  return tokens(r.warnings).map((w) => warningText(w, r)).join("; ");
}

/** Label and level for a run or profile: a succeeded record with warnings is never plain success. */
export function resultDisplay(record) {
  const r = obj(record);
  const warned = token(r.outcome) === "succeeded" && tokens(r.warnings).length > 0;
  return warned
    ? { label: "Succeeded with warnings", level: "warn", warned: true }
    : { label: outcomeLabel(r.outcome), level: outcomeLevel(r.outcome), warned: false };
}

/** "7 h", "40 min": a span, as opposed to ageLabel's "7 h ago". */
export function spanLabel(seconds) {
  const n = Number(seconds);
  if (!Number.isFinite(n) || n < 0) return "";
  if (n < 90) return "under a minute";
  if (n < 5400) return `${Math.round(n / 60)} min`;
  if (n < 172800) return `${Math.round(n / 3600)} h`;
  return `${Math.floor(n / 86400)} d`;
}

const HEARTBEAT_SOURCE_COPY = Object.freeze({
  io: "The last sign of life was the core reading or writing data.",
  progress: "The last sign of life was a change in the core's progress record.",
  liveness: "This runner can only tell that the core is alive, not that it is making progress, so a hang cannot be ruled out from here."
});

function ageLabel(epoch, nowSeconds) {
  const e = Number(epoch);
  if (!Number.isFinite(e) || e <= 0) return "";
  const s = Math.max(0, Math.floor(nowSeconds - e));
  if (s < 90) return "moments ago";
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  if (s < 172800) return `${Math.round(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} d ago`;
}

export const OUTCOMES_SCHEMA = "sdsync.dsm-outcomes.v1";

/**
 * "" when the block has the shape this UI reads, otherwise the reason it does
 * not. A block that is present but hollow (`{}`), of another schema, or with a
 * wrong-typed run outcome used to read as "All profiles healthy": every missing
 * member defaulted to "not run" or "ok". Absence of evidence is never green, so
 * anything short of the reviewed shape is "status unavailable" with its reason.
 */
export function outcomesProblem(raw) {
  if (raw === undefined || raw === null) return "this package did not report run outcomes, so failures cannot be ruled out";
  if (typeof raw !== "object" || Array.isArray(raw)) return "the run outcomes were not a document";
  if (raw.schema !== OUTCOMES_SCHEMA) {
    return `the run outcomes have schema ${typeof raw.schema === "string" && SAFE_TOKEN.test(raw.schema) ? raw.schema : "none"}, expected ${OUTCOMES_SCHEMA}`;
  }
  const run = raw.run;
  if (!run || typeof run !== "object" || Array.isArray(run)) return "the run outcomes have no run record";
  if (typeof run.outcome !== "string" || !run.outcome) return "the last run's outcome is missing or not text";
  if (!raw.alerts || typeof raw.alerts !== "object" || Array.isArray(raw.alerts)) return "the run outcomes have no alert state";
  if (!Array.isArray(raw.profiles)) return "the per-profile outcomes are not a list";
  if (raw.routines !== undefined && !Array.isArray(raw.routines)) return "the routine outcomes are not a list";
  if (raw.errors !== undefined && !Array.isArray(raw.errors)) return "the record-error list is not a list";
  if (raw.lasting !== undefined && !Array.isArray(raw.lasting)) return "the list of profiles held after a lasting failure is not a list";
  if (raw.engine !== undefined && (!raw.engine || typeof raw.engine !== "object" || Array.isArray(raw.engine))) return "the service engine record is not a document";
  if (raw.status_cache_health !== undefined && (!raw.status_cache_health || typeof raw.status_cache_health !== "object" || Array.isArray(raw.status_cache_health))) {
    return "the status-figure health is not a document";
  }
  return "";
}

/** The attention item for a snapshot whose outcomes block is absent or unreadable. */
export function outcomesUnavailableItem(raw) {
  const problem = outcomesProblem(raw);
  return {
    id: "outcomes-missing", level: "unknown", title: "Run outcomes unavailable",
    detail: `Status unavailable: ${problem || "the run outcomes could not be read"}. Update the package, or repair it so one complete release is installed.`
  };
}

/**
 * Validate and flatten the block. Returns null when the snapshot carries none
 * (an older package) or one of an unreviewed shape (see outcomesProblem), which
 * callers must show as "outcomes unavailable" rather than as success.
 */
export function normalizeOutcomes(raw) {
  if (outcomesProblem(raw)) return null;
  const run = obj(raw.run);
  const stale = obj(run.stale);
  const alerts = obj(raw.alerts);
  return {
    run: {
      outcome: token(run.outcome) || "not_run",
      reason: token(run.reason),
      operation: token(run.operation),
      scope: token(run.scope),
      startedEpoch: count(run.started_epoch),
      finishedEpoch: count(run.finished_epoch),
      exitCode: Number.isFinite(Number(run.exit_code)) && run.exit_code !== null ? Number(run.exit_code) : null,
      detail: typeof run.detail === "string" ? run.detail.replace(/[^ -~]/g, "?").slice(0, 160) : "",
      aggregate: run.aggregate && typeof run.aggregate === "object" && !Array.isArray(run.aggregate) ? {
        outcome: token(run.aggregate.outcome) || "not_run",
        reason: token(run.aggregate.reason),
        finishedEpoch: count(run.aggregate.finished_epoch)
      } : null,
      countsAvailable: typeof run.counts_available === "boolean" ? run.counts_available : null,
      warnings: tokens(run.warnings),
      // null when the bridge cannot say (a run record stamped ahead of the NAS clock): not zero.
      runningFor: run.running_for_seconds === null ? null : count(run.running_for_seconds),
      // true / false / null (the runner writes no heartbeat): no progress for 30 min.
      stalled: run.stalled === true,
      // null while running (no heartbeat epoch, liveness only, or a skewed clock): nothing can be
      // said either way, which is not the same as "not stuck".
      stallUnobservable: token(run.outcome) === "running" && (run.stalled === null || tokens(run.warnings).includes("stall_unobservable")),
      // When the runner last showed progress, if the bridge says (optional).
      heartbeatEpoch: count(run.heartbeat_epoch),
      // What the runner last saw advance: io (the core read or wrote), progress (its
      // progress record changed) or liveness (only that it is alive).
      heartbeatSource: ["io", "progress", "liveness"].includes(token(run.heartbeat_source)) ? token(run.heartbeat_source) : "",
      longRunning: run.long_running === true || tokens(run.warnings).includes("long_running"),
      stderrLog: run.stderr_log && typeof run.stderr_log === "object" && !Array.isArray(run.stderr_log) && count(run.stderr_log.finished_epoch) !== null
        ? { finishedEpoch: count(run.stderr_log.finished_epoch), exitCode: Number.isFinite(Number(run.stderr_log.exit_code)) ? Number(run.stderr_log.exit_code) : null }
        : null,
      ...factsOf(run),
      deferred: count(run.deferred_files),
      skipped: count(run.skipped_files),
      degraded: run.degraded && typeof run.degraded === "object" && !Array.isArray(run.degraded) ? {
        sinks: (Array.isArray(run.degraded.sinks) ? run.degraded.sinks : []).map((sink) => shown(token(sink))).slice(0, 8),
        reason: token(run.degraded.reason)
      } : null,
      staleDetected: stale.detected === true,
      lock: token(stale.lock)
    },
    alerts: {
      status: token(alerts.status) || "absent",
      failureCount: count(alerts.failure_count),
      threshold: count(alerts.failure_threshold),
      thresholdReached: alerts.threshold_reached === true
    },
    profiles: (Array.isArray(raw.profiles) ? raw.profiles : []).map((item) => {
      const p = obj(item);
      return {
        profile: token(p.profile),
        // A profile record whose outcome is missing or not text is unknown, which is
        // a failure level; "not run" would be idle grey and read as nothing to do.
        outcome: token(p.outcome) || "unknown",
        reason: token(p.reason),
        // The core's own error line and the path it failed on, written by the runner.
        // The path arrives percent-encoded.
        detail: typeof p.detail === "string" ? plainText(p.detail, 240) : "",
        failedPath: typeof p.failed_path === "string" ? visibleText(decodePercentPath(p.failed_path)).slice(0, 255) : "",
        action: token(p.action),
        finishedEpoch: count(p.finished_epoch),
        outcomeEpoch: count(p.outcome_epoch),
        failures: count(p.consecutive_failures),
        lastSuccessEpoch: count(p.last_success_epoch),
        cache: token(p.status_cache),
        countsAvailable: typeof p.counts_available === "boolean" ? p.counts_available : null,
        warnings: tokens(p.warnings),
        skipped: count(p.skipped_files),
        deferred: count(p.deferred_files),
        // The checksum (pair) index this profile's last run used, or null.
        index: normalizeIndex(p.index),
        ...factsOf(p)
      };
    }).filter((p) => p.profile),
    // Profiles held after a lasting failure; an older package sends none.
    lasting: normalizeLasting(raw.lasting),
    // Which service engine runs; null for a package that does not report it.
    engine: normalizeEngine(raw.engine),
    routines: (Array.isArray(raw.routines) ? raw.routines : []).map((item) => {
      const r = obj(item);
      return {
        profile: token(r.profile),
        deferReason: token(r.defer_reason),
        lastExit: Number.isFinite(Number(r.last_exit_code)) && r.last_exit_code !== null ? Number(r.last_exit_code) : null,
        lastReason: token(r.last_reason)
      };
    }).filter((r) => r.profile),
    // Whether a saved change is still waiting for the running controller to pick it
    // up: pending true / false, or null when the marker could not be read.
    controllerReload: (() => {
      const c = raw.controller_reload;
      if (!c || typeof c !== "object" || Array.isArray(c)) return { pending: false, operation: "", reason: "", epoch: null };
      return {
        pending: c.pending === true ? true : (c.pending === null ? null : false),
        operation: token(c.operation) ? shown(token(c.operation)) : "",
        reason: token(c.reason) ? shown(token(c.reason)) : "",
        epoch: count(c.epoch)
      };
    })(),
    cacheHealth: (() => {
      const h = obj(raw.status_cache_health);
      const issues = (Array.isArray(h.issues) ? h.issues : []).map((i) => ({
        code: shown(token(obj(i).code)),
        severity: token(obj(i).severity),
        cause: typeof obj(i).cause === "string" ? obj(i).cause.replace(/[^\x20-\x7e]/g, "?").slice(0, 160) : ""
      }));
      return { state: token(h.state), issues };
    })(),
    errors: (Array.isArray(raw.errors) ? raw.errors : []).map((item) => {
      const e = obj(item);
      return { source: shown(token(e.source)), code: shown(token(e.code)) };
    })
  };
}

/** Plain-language line for a run or profile outcome with its reason. */
export function outcomeSentence(outcome, reason) {
  const label = outcomeLabel(outcome);
  const why = reasonCopy(reason).text;
  return why ? `${label}: ${why}` : label;
}

/** What to say beside a tree-compare row so it never reads as a bare "in sync". */
export function statusCacheNote(entry) {
  const p = obj(entry);
  const cache = token(p.cache);
  if (!cache || cache === "trusted") return "";
  if (cache === "no_run_recorded") return "No run recorded for this profile, or the last all-profiles run did not reach it; showing the last tree comparison only.";
  const why = reasonCopy(p.reason).text;
  if (cache === "last_run_failed") return `Last run ${p.outcome === "cancelled" ? "cancelled" : "failed"}${why ? `: ${why}` : "."}`;
  if (cache === "skipped_entries") return `Last run synced with warnings: ${warningsSentence(p) || "some entries are left out until fixed"}.`;
  if (cache === "last_run_partial") return `Last run partial: some files are pending${why ? ` (${why})` : "."}`;
  return `Status trust ${unknownLabel(cache)}.`;
}

/** One row per profile for the AppWindow and the widget. */
export function profileOutcomeRow(entry, nowSeconds = Date.now() / 1000) {
  const p = obj(entry);
  const parts = [];
  const why = reasonCopy(p.reason).text;
  if (why) parts.push(why);
  if (p.failures > 1) parts.push(`failed ${p.failures} times in a row`);
  const age = ageLabel(p.outcomeEpoch || p.finishedEpoch, nowSeconds);
  // Not run because an aggregate stopped short is a skip, not idleness.
  const shownResult = resultDisplay(p);
  let level = p.outcome === "not_run" && p.reason ? "warn" : shownResult.level;
  // A reason this UI cannot read is never shown in a calm colour: the code is in
  // the detail, and the row is at least a warning until someone has looked.
  if (p.reason && !reasonCopy(p.reason).known && (level === "ok" || level === "idle")) level = "warn";
  if (tokens(p.warnings).length) parts.push(warningsSentence(p));
  if (p.detail) parts.push(`core said: ${p.detail}`);
  if (p.failedPath) parts.push(`failed on ${p.failedPath}`);
  return {
    profile: p.profile,
    outcome: token(p.outcome),
    label: shownResult.label,
    level,
    reason: token(p.reason),
    age,
    detail: parts.join("; "),
    failures: p.failures,
    note: statusCacheNote(p),
    indexNote: indexLine(p.index)
  };
}

/** "Status figures could not be read: <cause>" when the rollup store is degraded. */
export function cacheHealthNote(outcomes) {
  const h = outcomes && outcomes.cacheHealth;
  if (!h || !h.state || h.state === "ok") return "";
  if (h.state !== "degraded") return `Status figures health ${unknownLabel(h.state)}.`;
  const issue = h.issues.find((i) => i.severity === "warning") || h.issues[0];
  return `Status figures could not be read${issue && issue.cause ? `: ${issue.cause}` : "."}`;
}

/** "History starts <date>" so a rotated-away failure is not read as none. */
export function historyNote(history, formatDate) {
  if (!history || typeof history !== "object" || Array.isArray(history)) {
    return "History depth not reported by this package; an older failure may have been rotated away.";
  }
  if (history.error) return `History start unavailable (${shown(token(history.error))}); an older failure may have been rotated away.`;
  const epoch = Number(history.oldest_record_epoch);
  const files = count(history.files);
  if (!Number.isFinite(epoch) || epoch <= 0) return "No activity records yet.";
  return `History starts ${formatDate(epoch)}${files ? ` (${files} log file${files === 1 ? "" : "s"})` : ""}; anything older was rotated away.`;
}

/**
 * Everything that needs the operator's attention, worst first. An empty list
 * means the record shows nothing wrong, never that the record was missing:
 * callers pass `null` through `outcomesMissingItem` for that.
 */
export function attentionItems(outcomes, nowSeconds = Date.now() / 1000) {
  const items = [];
  if (!outcomes) return items;
  const run = outcomes.run;
  const stamp = ageLabel(run.finishedEpoch || run.startedEpoch, nowSeconds);
  const when = stamp ? ` (${stamp})` : "";
  if (run.outcome === "stale" || run.staleDetected) {
    items.push({ id: "run-stale", level: "fail", title: "A run died without finishing", detail: `The runner vanished, so the last run's result is unknown and counts as failed${when}. ${reasonCopy("runner_vanished").text}` });
  } else if (run.outcome === "failed") {
    items.push({ id: "run-failed", level: "fail", title: `Last run failed${outcomes.alerts.failureCount > 1 ? `, ${outcomes.alerts.failureCount} failures in a row` : ""}`, detail: `${reasonCopy(run.reason).text || "No cause was recorded. Review Logs."}${run.detail ? ` Detail: ${run.detail}.` : ""}${when}` });
  } else if (run.outcome === "partial") {
    const n = (run.deferred || 0) + (run.skipped || 0);
    items.push({ id: "run-partial", level: "warn", title: "Last run was partial", detail: `${n ? `${n} file${n === 1 ? "" : "s"} deferred or skipped` : (run.countsAvailable === false ? "Entry counts were not recorded for this run, so how many files were deferred or skipped is unknown" : "An unrecorded number of files were deferred or skipped")}${run.reason ? ` (${reasonCopy(run.reason).text})` : ""}; they are left for the next run${when}.` });
  } else if (run.outcome === "succeeded" && run.warnings.length) {
    items.push({ id: "run-warnings", level: "warn", title: "Last run synced with warnings", detail: `${warningsSentence(run)}. Skipped and deferred entries are listed as entry.skipped / entry.deferred records in the sync log${when}.` });
  } else if (run.outcome === "running" && run.longRunning) {
    items.push({ id: "run-long", level: "warn", title: `A run has been going for ${run.runningFor !== null ? spanLabel(run.runningFor) : "a long time"}`, detail: "Check whether it is stuck: a core waiting on an unresponsive source or target never fails, it just never finishes. Review Logs, and stop the package in Package Center, then Run it again if nothing is moving." });
  } else if (run.outcome === "busy") {
    items.push({ id: "run-busy", level: "warn", title: "Last run was refused as busy", detail: `${reasonCopy("lock_busy").text}${when}` });
  } else if (run.outcome === "cancelled") {
    items.push({ id: "run-cancelled", level: "warn", title: "Last run was cancelled", detail: `Nothing was synced after the stop${when}. A routine runs again on its own; a run you started is not repeated.` });
  } else if (run.outcome === "unknown") {
    items.push({ id: "run-unknown", level: "fail", title: "Last run status unknown", detail: `The package could not interpret the last run record, so it counts as failed${when}. Review Logs.` });
  } else if (!Object.prototype.hasOwnProperty.call(OUTCOME_LABELS, run.outcome)) {
    items.push({ id: "run-unknown", level: "unknown", title: `Last run ${unknownLabel(run.outcome)}`, detail: "This UI does not recognise the run outcome. Review Logs." });
  }
  if (run.reason && !reasonCopy(run.reason).known && ["succeeded", "not_run", "running", "busy", "cancelled"].includes(run.outcome)) {
    items.push({ id: "run-reason-unknown", level: "warn", title: "The last run reported a reason this UI does not recognise", detail: `${reasonCopy(run.reason).text}. Review Logs.${when}` });
  }
  if (run.outcome === "running" && run.stalled) {
    const quiet = run.heartbeatEpoch ? spanLabel(Math.max(0, nowSeconds - run.heartbeatEpoch)) : "";
    const how = HEARTBEAT_SOURCE_COPY[run.heartbeatSource] ? ` ${HEARTBEAT_SOURCE_COPY[run.heartbeatSource]}` : "";
    items.push({ id: "run-stalled", level: "warn", title: quiet ? `Run may be stuck: no progress for ${quiet}` : "Run may be stuck: no progress for 30 minutes", detail: `The runner's heartbeat has stopped advancing. Check whether it is stuck on an unresponsive source or target: review Logs, and stop the package in Package Center, then Run it again if nothing is moving.${how}` });
  }
  if (outcomes.controllerReload.pending === true) {
    const reload = outcomes.controllerReload;
    const when = reload.epoch ? ` (${ageLabel(reload.epoch, nowSeconds)})` : "";
    const why = reload.reason ? reasonCopy(reload.reason).text : "";
    items.push({ id: "controller-reload", level: "warn", title: "Saved, not yet applied; stop and run the package service", detail: `A change${reload.operation ? ` (${reload.operation})` : ""} was saved${when}, but the running controller was not told to reload it, so it keeps the previous settings${why ? `: ${why.split(". ")[0]}` : ""}. Stop the package in Package Center, then Run it again.` });
  } else if (outcomes.controllerReload.pending === null) {
    items.push({ id: "controller-reload", level: "warn", title: "Cannot tell whether saved changes are applied", detail: "The marker that records a pending controller reload could not be read. If a recent save did not take effect, stop the package in Package Center, then Run it again." });
  }
  if (run.warnings.some((w) => w !== "long_running" && w !== "stalled" && w !== "stall_unobservable") && run.outcome !== "succeeded") {
    items.push({ id: "run-warnings-extra", level: "warn", title: "The last run also reported warnings", detail: `${warningsSentence(Object.assign({}, run, { warnings: run.warnings.filter((w) => w !== "long_running" && w !== "stalled" && w !== "stall_unobservable") }))}${when}.` });
  }
  if (run.degraded) {
    const sinks = run.degraded.sinks.join(", ");
    items.push({ id: "run-degraded", level: "warn", title: "Some records of this run could not be written", detail: `${sinks || "unspecified records"}${run.degraded.reason ? ` (${shown(run.degraded.reason)})` : ""}. The run's own result is trustworthy; check Logs for the missing echoes.` });
  }
  const agg = run.aggregate;
  if (agg && run.scope !== "all" && ["failed", "partial", "unknown", "busy", "cancelled"].includes(agg.outcome)) {
    const bad = agg.outcome === "failed" || agg.outcome === "unknown";
    items.push({ id: "aggregate", level: bad ? "fail" : "warn", title: `Last all-profiles run ${outcomeLabel(agg.outcome).toLowerCase()}`, detail: `${reasonCopy(agg.reason).text || "Review Logs."}${ageLabel(agg.finishedEpoch, nowSeconds) ? ` (${ageLabel(agg.finishedEpoch, nowSeconds)})` : ""} A later single-profile run does not undo it.` });
  }
  for (const p of outcomes.profiles) {
    const row = profileOutcomeRow(p, nowSeconds);
    if (row.level === "fail" || row.level === "unknown" || (row.level === "warn" && p.outcome !== "busy")) {
      items.push({ id: `profile-${p.profile}`, level: row.level, title: `${visibleText(p.profile)}: ${row.label}`, detail: `${row.detail || "Review Logs."}${row.age ? ` (${row.age})` : ""}` });
    }
  }
  for (const p of outcomes.profiles) {
    const item = indexAttentionItem(p);
    if (item) items.push(item);
  }
  for (const held of outcomes.lasting) items.push(lastingAttentionItem(held));
  const engineItem = engineAttentionItem(outcomes.engine);
  if (engineItem) items.push(engineItem);
  for (const r of outcomes.routines) {
    if (r.deferReason && !["window", "busy"].includes(r.deferReason)) {
      const why = routineDeferCopy(r.deferReason);
      items.push({
        id: `routine-${r.profile}`, level: why.level,
        title: why.blocked ? `Routine ${visibleText(r.profile)} is blocked` : `Routine ${visibleText(r.profile)} is deferred`,
        detail: why.text
      });
    } else if (r.lastExit !== null && r.lastExit !== 0 && ![3, 75, 130, 143].includes(r.lastExit) && !r.deferReason) {
      items.push({ id: `routine-${r.profile}`, level: "warn", title: `Routine ${visibleText(r.profile)} last exited ${r.lastExit}`, detail: reasonCopy(r.lastReason).text || "Review Logs." });
    }
  }
  const cacheNote = cacheHealthNote(outcomes);
  if (cacheNote) items.push({ id: "status-cache", level: "warn", title: "Stored status figures are degraded", detail: cacheNote });
  const a = outcomes.alerts;
  if (a.status === "unreadable") {
    items.push({ id: "alerts-unreadable", level: "fail", title: "Failure alerts unavailable", detail: "The alert state file is unreadable, so failure notifications are disabled. Repair or reinstall the package." });
  } else if (a.status !== "ok" && a.status !== "absent") {
    items.push({ id: "alerts-unknown", level: "unknown", title: `Failure alerts ${unknownLabel(a.status)}`, detail: "Review Logs." });
  } else if (a.failureCount > 0) {
    items.push({ id: "alerts-count", level: "warn", title: `Sync has failed ${a.failureCount} time${a.failureCount === 1 ? "" : "s"} in a row`, detail: a.thresholdReached ? "The alert threshold has been reached." : `Alerts fire at ${a.threshold || 1}.` });
  }
  if (outcomes.errors.length) {
    const named = outcomes.errors.slice(0, 3).map((e) => `${e.source} ${e.code}${RECORD_ERROR_COPY[e.code] ? ` (${RECORD_ERROR_COPY[e.code]})` : ""}`).join(", ");
    items.push({ id: "record-errors", level: "warn", title: "Package record-keeping is degraded", detail: `${outcomes.errors.length} record${outcomes.errors.length === 1 ? "" : "s"} could not be read: ${named}${outcomes.errors.length > 3 ? ", …" : ""}. Results above may be incomplete.` });
  }
  // The ids are v-for keys; two profiles or routines with one id would collide.
  const seen = new Map();
  return items.map((item) => {
    const count = seen.get(item.id) || 0;
    seen.set(item.id, count + 1);
    return count ? Object.assign({}, item, { id: `${item.id}#${count}` }) : item;
  });
}

/**
 * How long the package controller may go without republishing its state before
 * it is wedged or gone rather than busy.
 *
 * The controller rewrites `controller.updated_epoch` on every pass of its loop and
 * its longest idle sleep is thirty seconds, so six missed ticks is a daemon that
 * is wedged or gone. Staleness is read only while nothing is active: a controller
 * executing a long job publishes its active PID and then blocks for as long as
 * that job takes, and reading staleness there would have a four-hour sync report
 * its own controller as dead.
 */
export const CONTROLLER_TICK_STALE_SECONDS = 180;

/**
 * The service and the controller's own heartbeat as one attention item, or null.
 *
 * `serviceState` is the snapshot's `service.state` (running, stopped, untrusted);
 * `generatedEpoch` optionally overrides the snapshot's `generated_at_epoch`.
 * Nothing is syncing when the service is down, and nothing is scheduled when a
 * running controller has stopped checking in, so a green "Succeeded" from last
 * night must not be the only thing a reader sees. Both clocks here are the
 * package's: `generated_at_epoch` and `controller.updated_epoch` are written by
 * the NAS, so the browser's clock never enters.
 */
export function serviceAttentionItem(snapshot, serviceState, generatedEpoch) {
  if (!snapshot || typeof snapshot !== "object" || Array.isArray(snapshot)) return null;
  const state = token(serviceState);
  if (state === "untrusted") {
    return { id: "service-untrusted", level: "fail", title: "Service identity untrusted", detail: "The process holding the controller's PID file failed its identity check, so nothing is servicing routines or queued requests. Stop the package in Package Center, then Run it again, and review Logs." };
  }
  if (state !== "running") {
    const named = state === "stopped" ? "Package service stopped" : `Package service ${state ? shown(state) : "state unknown"}`;
    return { id: "service-down", level: "warn", title: named, detail: "Routines and schedules do not run, and queued requests wait, until the service is running. Run the package in Package Center." };
  }
  const controller = obj(snapshot.controller);
  // The AppWindow keeps its snapshot frozen and tracks the package clock beside it, so the
  // caller passes the live reading; without one the document's own is used.
  const generated = Number(generatedEpoch > 0 ? generatedEpoch : snapshot.generated_at_epoch);
  const updated = Number(controller.updated_epoch);
  const activePid = Number(controller.active_pid);
  if (Object.keys(controller).length && generated > 0 && updated > 0 && !(activePid > 0) && generated - updated >= CONTROLLER_TICK_STALE_SECONDS) {
    return { id: "controller-silent", level: "warn", title: `The controller has not checked in for ${spanLabel(generated - updated)}`, detail: "The controller process exists but is running nothing and has stopped reporting, so scheduled syncs may not be happening. Review Logs, and stop the package in Package Center, then Run it again if it does not recover." };
  }
  return null;
}

/** Shown when the snapshot has no outcomes block: absence is never green. */
export const OUTCOMES_MISSING_ITEM = Object.freeze(outcomesUnavailableItem(undefined));

/**
 * Whether a run is in flight. The legacy `run.state` stays "running" after the
 * runner dies, until the controller rewrites it, and a stopped controller never
 * does; so a stale verdict in the outcomes block always wins over it. Otherwise
 * either record saying "running" is enough. Callers pass the normalised block, or
 * null when there is none and the legacy state is all there is to go on.
 */
export function runIsActive(outcomes, legacyState) {
  if (runIsStale(outcomes)) return false;
  return legacyState === "running" || Boolean(outcomes && outcomes.run.outcome === "running");
}

/** The verdict word for the run: "stale" beats the legacy state everywhere. */
export function runIsStale(outcomes) {
  return Boolean(outcomes) && (outcomes.run.outcome === "stale" || outcomes.run.staleDetected);
}

/** Whether a dead run covers this profile, so its row cannot read as healthy or running. */
export function staleRunCovers(outcomes, profileName) {
  if (!runIsStale(outcomes)) return false;
  const scope = outcomes.run.scope;
  return !scope || scope === "none" || scope === "all" || scope === String(profileName);
}

export function worstLevel(items) {
  const order = ["fail", "unknown", "warn"];
  for (const level of order) if (items.some((i) => i.level === level)) return level;
  return "ok";
}

/**
 * Key of the newest failure the snapshot proves, for one-shot announcement.
 * Derived from state alone, so a failure that happened while no window was open
 * is still announced the first time a window loads.
 */
export function failureKey(outcomes) {
  if (!outcomes) return "";
  const run = outcomes.run;
  if (run.outcome === "failed" || run.outcome === "stale" || run.outcome === "unknown" || run.staleDetected) {
    return ["run", run.scope, run.finishedEpoch || run.startedEpoch || 0, run.exitCode, run.reason].join(":");
  }
  const agg = run.aggregate;
  if (agg && ["failed", "stale", "unknown"].includes(agg.outcome)) return ["aggregate", agg.finishedEpoch || 0, agg.reason].join(":");
  const bad = outcomes.profiles.find((p) => p.outcome === "failed" || p.outcome === "stale" || p.outcome === "unknown");
  return bad ? ["profile", bad.profile, bad.finishedEpoch || 0, bad.reason].join(":") : "";
}
