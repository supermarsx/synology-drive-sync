/**
 * Pure derivation of what the dashboard says about run outcomes.
 *
 * The package bridge attaches an `outcomes` block (schema
 * `sdsync.dsm-outcomes.v1`) to every snapshot. Nothing may fail silently, so
 * this module turns that block into copy that names the cause and the next
 * step, and every value it does not recognise is shown as `unknown (<token>)`
 * instead of being hidden or thrown on. Imports nothing, so the widget, the
 * AppWindow and the tests all read the same rules.
 */

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
  lock_busy: ["another run, Doctor or management action held the run lock", "Nothing to do; the run is retried."],
  runner_died: ["the runner died before it finished (killed, out of memory or power loss)", "Review Logs, then run again."],
  core_partial: ["the run changed the destination and then failed part-way", "Review Logs, then run again; nothing is rolled back."],
  runner_vanished: ["the runner died before it finished (killed, out of memory or power loss)", "Review Logs, then run again."],
  supervisor_refused: ["the package supervisor refused the sync command", "Review Logs and the package version."],
  dependency_unmet: ["a routine this one depends on has not succeeded", "Fix the dependency routine first."],
  outside_window: ["the routine is outside its daily window", "It runs when the window opens."],
  cancelled: ["the run was stopped", "Run again when ready."],
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
  config_invalid: ["the configuration is invalid", "Correct it on the Profiles page."],
  https_required: ["the target must use HTTPS", "Change the target to HTTPS."],
  scope_not_found: ["the named profile does not exist", "Check the profile name."],
  unclassified: ["the failure has no specific classification", "Review Logs."],
  changed_during_run: ["files changed during the run", "They sync next run."],
  delete_bound: ["deletions were bounded", "The rest is deleted next run."],
  server_copy_fallback: ["server-side copy was refused", "Files re-upload on the next run."],
  excluded: ["files were excluded", "Check the exclude rules if unexpected."],
  upload_failed: ["some uploads failed", "They retry on the next run."],
  "source-changed": ["files changed during the run", "They sync next run."],
  "source-vanished": ["a source file disappeared during the run", "Nothing to do unless it was unintended."],
  "source-unreadable": ["a source file could not be read", "Check its permissions."],
  "unsupported-local-entry": ["an unsupported local entry was left out", "Remove or exclude it."],
  "remote-digest-unavailable": ["the target gave no digest to verify against", "It is re-checked on the next run."],
  "remote-changed": ["the destination changed during the run", "Run again."],
  "unrepresentable-name": ["a file name cannot be represented on the target", "Rename it."],
  "remote-unreadable": ["the NAS does not let this account read this file, so it is never compared or deleted", "Grant the sync account read permission on it."],
  deferred_paths: ["some paths were deferred", "They sync on the next run; review the entry.deferred records in the sync log."],
  "listing-incomplete": ["the destination listed fewer or more entries than expected, so deletions in those folders are withheld", "Run again; review Logs if it repeats."],
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

function ageLabel(epoch, nowSeconds) {
  const e = Number(epoch);
  if (!Number.isFinite(e) || e <= 0) return "";
  const s = Math.max(0, Math.floor(nowSeconds - e));
  if (s < 90) return "moments ago";
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  if (s < 172800) return `${Math.round(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} d ago`;
}

/**
 * Validate and flatten the block. Returns null when the snapshot carries none
 * (an older package), which callers must show as "outcomes unavailable" rather
 * than as success.
 */
export function normalizeOutcomes(raw) {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
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
        outcome: token(p.outcome) || "not_run",
        reason: token(p.reason),
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
        ...factsOf(p)
      };
    }).filter((p) => p.profile),
    routines: (Array.isArray(raw.routines) ? raw.routines : []).map((item) => {
      const r = obj(item);
      return {
        profile: token(r.profile),
        deferReason: token(r.defer_reason),
        lastExit: Number.isFinite(Number(r.last_exit_code)) && r.last_exit_code !== null ? Number(r.last_exit_code) : null,
        lastReason: token(r.last_reason)
      };
    }).filter((r) => r.profile),
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
  const level = p.outcome === "not_run" && p.reason ? "warn" : shownResult.level;
  if (tokens(p.warnings).length) parts.push(warningsSentence(p));
  return {
    profile: p.profile,
    outcome: token(p.outcome),
    label: shownResult.label,
    level,
    reason: token(p.reason),
    age,
    detail: parts.join("; "),
    failures: p.failures,
    note: statusCacheNote(p)
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
  } else if (run.outcome === "busy") {
    items.push({ id: "run-busy", level: "warn", title: "Last run was refused as busy", detail: `${reasonCopy("lock_busy").text}${when}` });
  } else if (run.outcome === "cancelled") {
    items.push({ id: "run-cancelled", level: "warn", title: "Last run was cancelled", detail: `Nothing was synced after the stop${when}.` });
  } else if (run.outcome === "unknown") {
    items.push({ id: "run-unknown", level: "fail", title: "Last run status unknown", detail: `The package could not interpret the last run record, so it counts as failed${when}. Review Logs.` });
  } else if (!Object.prototype.hasOwnProperty.call(OUTCOME_LABELS, run.outcome)) {
    items.push({ id: "run-unknown", level: "unknown", title: `Last run ${unknownLabel(run.outcome)}`, detail: "This UI does not recognise the run outcome. Review Logs." });
  }
  if (run.warnings.length && run.outcome !== "succeeded") {
    items.push({ id: "run-warnings-extra", level: "warn", title: "The last run also reported warnings", detail: `${warningsSentence(run)}${when}.` });
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
      items.push({ id: `profile-${p.profile}`, level: row.level, title: `${p.profile}: ${row.label}`, detail: `${row.detail || "Review Logs."}${row.age ? ` (${row.age})` : ""}` });
    }
  }
  for (const r of outcomes.routines) {
    if (r.deferReason && !["window", "busy"].includes(r.deferReason)) {
      const why = has(REASONS, r.deferReason) ? reasonCopy(r.deferReason).text : `reason ${unknownLabel(r.deferReason)}`;
      items.push({ id: `routine-${r.profile}`, level: "warn", title: `Routine ${r.profile} is deferred`, detail: why });
    } else if (r.lastExit !== null && r.lastExit !== 0 && ![3, 75, 130, 143].includes(r.lastExit) && !r.deferReason) {
      items.push({ id: `routine-${r.profile}`, level: "warn", title: `Routine ${r.profile} last exited ${r.lastExit}`, detail: reasonCopy(r.lastReason).text || "Review Logs." });
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
    const named = outcomes.errors.slice(0, 3).map((e) => `${e.source} ${e.code}`).join(", ");
    items.push({ id: "record-errors", level: "warn", title: "Package record-keeping is degraded", detail: `${outcomes.errors.length} record${outcomes.errors.length === 1 ? "" : "s"} could not be read: ${named}${outcomes.errors.length > 3 ? ", …" : ""}. Results above may be incomplete.` });
  }
  return items;
}

/** Shown when the snapshot has no outcomes block: absence is never green. */
export const OUTCOMES_MISSING_ITEM = Object.freeze({
  id: "outcomes-missing", level: "unknown", title: "Run outcomes unavailable",
  detail: "This package did not report run outcomes, so failures cannot be ruled out. Update the package."
});

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
