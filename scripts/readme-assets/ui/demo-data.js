"use strict";
/* Neutral demo data for the README screenshot: what the package's read-only API would report for
 * a small NAS. Every name, path and host below is made up. The shapes are the ones the bundle's own
 * validators accept (sdsync.dsm-api.v1, sdsync.dsm-outcomes.v1). */

// 2026-11-16 09:41:12 UTC, the moment the screenshot pretends to be taken.
const NOW = Date.UTC(2026, 10, 16, 9, 41, 12) / 1000;
const MIN = 60;
const HOUR = 3600;

function snapshot() {
  const lastFinished = NOW - 3 * MIN - 9;
  return {
    schema: "sdsync.dsm-api.v1",
    generated_at_epoch: NOW,
    service: { state: "running", pid: 4242 },
    package: { version: "26.66", arch: "x86_64", target: "x86_64-unknown-linux-musl" },
    capabilities: { mutations: true, secrets: true, write_test: true },
    profiles: [
      {
        name: "documents", source: "/volume1/Documents", remote: "/home/Backup/documents",
        url: "https://nas-b.example.com", username: "deposit-bot", has_password: true, has_totp: false,
        has_remote_log_token: false, compare: "content", jobs: 2, delete: false, max_delete: 0, is_default: true
      },
      {
        name: "backups", source: "/volume1/Backups", remote: "/TeamShare/backups",
        url: "https://nas-b.example.com", username: "deposit-bot", has_password: true, has_totp: false,
        has_remote_log_token: false, compare: "content", jobs: 2, delete: false, max_delete: 0
      }
    ],
    routines: [
      {
        profile: "documents", enabled: true, action: "sync", mode: "realtime", debounce_seconds: 15,
        poll_seconds: 60, backend: "inotify", backend_reason: "ok", observe_state: "observing",
        last_success_epoch: lastFinished, next_run_epoch: 0, last_exit_code: 0, state: "succeeded"
      },
      {
        profile: "backups", enabled: true, action: "sync", mode: "daily", window_start: "02:00", window_end: "04:00",
        weekdays: [1, 2, 3, 4, 5, 6, 7], last_success_epoch: NOW - 7 * HOUR - 30 * MIN,
        next_run_epoch: Date.UTC(2026, 10, 17, 2, 0, 0) / 1000, last_exit_code: 0, state: "succeeded"
      }
    ],
    run: {
      operation: "sync", scope: "documents", status: "success",
      started_epoch: lastFinished - 4, finished_epoch: lastFinished
    },
    outcomes: {
      schema: "sdsync.dsm-outcomes.v1",
      run: {
        outcome: "succeeded", operation: "sync", scope: "documents", reason: "",
        started_epoch: lastFinished - 4, finished_epoch: lastFinished, exit_code: 0,
        counts_available: true, warnings: [], stalled: false
      },
      alerts: { status: "ok", failure_count: 0, failure_threshold: 3, threshold_reached: false },
      profiles: [
        { profile: "documents", outcome: "succeeded", reason: "", action: "sync", finished_epoch: lastFinished,
          outcome_epoch: lastFinished, consecutive_failures: 0, last_success_epoch: lastFinished, counts_available: true },
        { profile: "backups", outcome: "succeeded", reason: "", action: "sync", finished_epoch: NOW - 7 * HOUR - 30 * MIN,
          outcome_epoch: NOW - 7 * HOUR - 30 * MIN, consecutive_failures: 0, last_success_epoch: NOW - 7 * HOUR - 30 * MIN, counts_available: true }
      ],
      routines: [],
      lasting: [],
      engine: { active: "daemon", file: "absent", detail: "" }
    },
    alerts: { enabled: true, failure_threshold: 3, cooldown_seconds: 3600 },
    security_policy: {}
  };
}

function csrf() {
  const issued = NOW;
  return { schema: "sdsync.dsm-csrf.v1", ok: true, csrf_token: `v1.${issued}.${issued + 300}.demo.demo` };
}

module.exports = { NOW, snapshot, csrf };
