import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

const source = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const apiSource = await readFile(new URL("../src/api.js", import.meta.url), "utf8");
const makefile = await readFile(new URL("../Makefile", import.meta.url), "utf8");

function jsonResponse(model, status = 200) {
  return {
    redirected: false,
    status,
    ok: status >= 200 && status < 300,
    headers: { get: (name) => name.toLowerCase() === "content-type" ? "application/json" : null },
    async text() { return JSON.stringify(model); }
  };
}

async function loadApi() {
  const encoded = Buffer.from(apiSource).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${Date.now()}-${Math.random()}`);
}

function installBrowserGlobals() {
  const previous = { window: globalThis.window, fetch: globalThis.fetch };
  globalThis.window = {
    crypto: globalThis.crypto || webcrypto,
    TextEncoder: globalThis.TextEncoder,
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout
  };
  return () => {
    if (previous.window === undefined) delete globalThis.window;
    else globalThis.window = previous.window;
    if (previous.fetch === undefined) delete globalThis.fetch;
    else globalThis.fetch = previous.fetch;
  };
}

test("Doctor awaits terminal bridge evidence while plan and run remain queued", () => {
  assert.match(source, /const awaitTerminal = kind === "doctor";/);
  assert.match(
    source,
    /ACTIONS\.execute,[\s\S]*?Object\.assign\(\{ kind \}, payload\),[\s\S]*?awaitTerminal/
  );
  assert.match(source, /this\.diagnostic = \{ title: "Doctor completed", output: message \}/);
  assert.match(source, /result\.output \|\| result\.message/);
  assert.match(source, /error\.resultOutput \|\| report\.message/);
  assert.match(source, /title: report\.unknown \? "Doctor outcome unknown" : "Doctor failed"/);
  assert.match(source, /quickPlan\(\).*executeOperation\("plan"/);
  assert.match(source, /executeOperation\("run"/);
});

test("queued terminal polling outlives the former two-minute attempt horizon", async () => {
  const restore = installBrowserGlobals();
  try {
    const api = await loadApi();
    const jobId = "a".repeat(48);
    let resultReads = 0;
    let requestId = "";
    globalThis.fetch = async (url, options) => {
      if (options.method === "POST") {
        const request = JSON.parse(options.body);
        requestId = request.request_id;
        assert.equal(url, api.API_URL);
        assert.equal(options.credentials, "same-origin");
        assert.equal(options.headers["X-SDSYNC-Request"], "1");
        return jsonResponse({
          schema: api.QUEUED_SCHEMA,
          ok: true,
          state: "queued",
          request_id: request.request_id,
          job_id: jobId
        });
      }

      resultReads += 1;
      if (resultReads <= 3) throw new TypeError("temporary DSM transport failure");
      if (resultReads <= 245) {
        return jsonResponse({
          schema: api.RESULT_STATUS_SCHEMA,
          ok: true,
          state: "pending",
          job_id: jobId
        });
      }
      return jsonResponse({
        schema: api.RESULT_STATUS_SCHEMA,
        ok: true,
        state: "complete",
        job_id: jobId,
        client_request_id: requestId,
        result: {
          schema: api.RESULT_SCHEMA,
          ok: true,
          status: "succeeded",
          message: "Doctor completed",
          output: "bounded terminal Doctor evidence",
          exit_code: 0
        }
      });
    };

    const result = await api.apiPost(
      { signal: new AbortController().signal },
      "csrf-token",
      api.ACTIONS.execute,
      { allow_delete: null, kind: "doctor", max_total_delete: null, scope: "all", write_test: false },
      true,
      0
    );

    assert.equal(result.output, "bounded terminal Doctor evidence");
    assert.ok(resultReads > 240, `expected more than 240 result observations, received ${resultReads}`);
  } finally {
    restore();
  }
});

test("only an actual terminal failure is reported as failed and its output is preserved", async () => {
  const restore = installBrowserGlobals();
  try {
    const api = await loadApi();
    const jobId = "b".repeat(48);
    let requestId = "";
    globalThis.fetch = async (_url, options) => {
      if (options.method === "POST") {
        const request = JSON.parse(options.body);
        requestId = request.request_id;
        return jsonResponse({
          schema: api.QUEUED_SCHEMA,
          ok: true,
          state: "queued",
          request_id: request.request_id,
          job_id: jobId
        });
      }
      return jsonResponse({
        schema: api.RESULT_STATUS_SCHEMA,
        ok: true,
        state: "complete",
        job_id: jobId,
        client_request_id: requestId,
        result: {
          schema: api.RESULT_SCHEMA,
          ok: false,
          status: "failed",
          message: "Doctor rejected the target",
          output: "exact bounded terminal failure evidence",
          exit_code: 1
        }
      });
    };

    await assert.rejects(
      api.apiPost(
        { signal: new AbortController().signal },
        "csrf-token",
        api.ACTIONS.execute,
        { allow_delete: null, kind: "doctor", max_total_delete: null, scope: "all", write_test: false },
        true,
        0
      ),
      (error) => {
        assert.equal(error.outcomeUnknown, undefined);
        assert.equal(error.message, "Doctor rejected the target");
        assert.equal(error.resultOutput, "exact bounded terminal failure evidence");
        assert.equal(error.operation, api.ACTIONS.execute);
        return true;
      }
    );
  } finally {
    restore();
  }
});

test("repeated queued-result observation errors become outcome unknown, not failed", async () => {
  const restore = installBrowserGlobals();
  try {
    const api = await loadApi();
    const jobId = "c".repeat(48);
    let resultReads = 0;
    globalThis.fetch = async (_url, options) => {
      if (options.method === "POST") {
        const request = JSON.parse(options.body);
        return jsonResponse({
          schema: api.QUEUED_SCHEMA,
          ok: true,
          state: "queued",
          request_id: request.request_id,
          job_id: jobId
        });
      }
      resultReads += 1;
      throw new TypeError("DSM result bridge unavailable");
    };

    await assert.rejects(
      api.apiPost(
        { signal: new AbortController().signal },
        "csrf-token",
        api.ACTIONS.alertPolicy,
        { cooldown_seconds: 3600, enabled: true, failure_threshold: 1, on_failure: true, on_success: false },
        true,
        0
      ),
      (error) => {
        assert.equal(error.name, "QueuedOutcomeUnknownError");
        assert.equal(error.outcomeUnknown, true);
        assert.equal(error.jobId, jobId);
        assert.match(error.message, /accepted.*cannot currently be observed/i);
        return true;
      }
    );
    assert.equal(resultReads, 5);
  } finally {
    restore();
  }
});

test("AppWindow overlays, focus behavior, labels, secrets, and mutation guards remain native and scoped", () => {
  const mainClose = source.indexOf("</main>");
  assert.ok(mainClose >= 0);
  assert.ok(source.indexOf('class="sdsync-toasts"') > mainClose);
  assert.ok(source.indexOf('class="sdsync-modal-backdrop"') > mainClose);
  assert.match(source, /ref="confirmationDialog"[\s\S]*?role="dialog"[\s\S]*?aria-modal="true"/);
  assert.match(source, /document\.addEventListener\("keydown", this\.confirmationKeyHandler, true\)/);
  assert.match(source, /document\.removeEventListener\("keydown", this\.confirmationKeyHandler, true\)/);
  assert.match(source, /event\.key === "Escape"/);
  assert.match(source, /event\.key !== "Tab"/);
  assert.match(source, /priorFocus\.isConnected && priorFocus\.focus/);
  assert.match(source, /:aria-label="item\.title"/);
  assert.match(source, /aria-label="Window starts"/);
  assert.match(source, /aria-label="Window ends"/);
  assert.match(source, /aria-label="Wait for routines"/);
  assert.match(source, /this\.route === "profiles" && route !== "profiles"\) \{[\s\S]*?this\.profileSaveState === "saving" \|\| this\.profileConnectionState === "testing"[\s\S]*?return;[\s\S]*?this\.closeProfile\(\)/);
  assert.match(source, /clearSecrets\(\) \{ this\.secretValues = \{ password: "", totp: "", remote_log_token: "" \}; \}/);
  assert.match(source, /removeProfile\(\)[\s\S]*?this\.operationBusy\) return/);
  assert.match(source, /removeRoutine\(\)[\s\S]*?this\.operationBusy\) return/);
  assert.match(source, /:disabled="!canRunOperations \|\| !profiles\.length \|\| operationBusy"/);
  assert.match(source, /let configurationApplied = false;\s*let activeSecretKind = "";\s*const appliedSecretKinds = \[\];/);
  assert.match(source, /ACTIONS\.configureProfile, payload, true, undefined, AUTOSAVE_API_LIMITS\);\s*configurationApplied = true;/);
  assert.match(source, /activeSecretKind = secret\.kind;[\s\S]*?const secretResult = await apiPost\([\s\S]*?ACTIONS\.setSecret, secret, true, undefined, AUTOSAVE_API_LIMITS\);\s*this\.applyTrustedSecretPresence\(secretResult\);\s*appliedSecretKinds\.push\(secret\.kind\)/);
  assert.match(source, /partiallyApplied \? "Profile partially applied" : "Profile not saved"/);
  assert.match(source, /Do not retry this multi-stage operation; inspect Activity and Logs/);
  assert.match(source, /if \(configurationApplied && !this\.selectedProfile\) this\.selectedProfile = payload\.name;[\s\S]*?this\.profileSaveState = "error";[\s\S]*?profile editor was preserved/);
});

test("unbounded polling bounds each attempt and has a five-failure ceiling while bounded callers own the overall budget", () => {
  assert.match(apiSource, /\/webapi\/entry\.cgi\?api=SYNO\.API\.Auth&version=6&method=token/);
  assert.match(apiSource, /authenticated\["X-SYNO-TOKEN"\] = dsmAuth\.token/);
  assert.match(apiSource, /apiGetWithDsmAuth\([\s\S]*?"result",[\s\S]*?\{ job_id: jobId \},[\s\S]*?dsmAuth,[\s\S]*?true,[\s\S]*?attempt\.signal[\s\S]*?\)/);
  assert.doesNotMatch(
    apiSource,
    /consumeLaunchToken|launch token|window\.location|window\.history|history\.replaceState|localStorage|sessionStorage|document\.cookie/i
  );
  assert.match(apiSource, /credentials: "same-origin"/);
  assert.match(apiSource, /"X-SDSYNC-Request": "1"/);
  assert.doesNotMatch(apiSource, /RESULT_POLL_ATTEMPTS|within two minutes/);
  assert.match(apiSource, /for \(;;\)/);
  assert.match(apiSource, /const limits = boundedObservationLimits \|\| terminalAttemptLimits\(\)/);
  assert.match(apiSource, /if \(!observation && consecutiveObservationFailures >= RESULT_POLL_OBSERVATION_FAILURES\)/);
  assert.match(apiSource, /const RESULT_POLL_OBSERVATION_FAILURES = 5/);
  assert.match(apiSource, /const result = await awaitQueuedResult\([\s\S]*?id,[\s\S]*?action,[\s\S]*?limits,[\s\S]*?boundedObservationLimits[\s\S]*?\)/);
  assert.match(apiSource, /pollJobResult\([\s\S]*?requestId,[\s\S]*?limits,[\s\S]*?observation,[\s\S]*?operation[\s\S]*?\)[\s\S]*?limits\.resultObservationTimeoutMs/);
  assert.match(apiSource, /observation\.expired = true;[\s\S]*?observation\.cancelCurrent\(\)/);
  assert.match(apiSource, /if \(auth && auth\.signal && auth\.signal\.aborted\) throw error/);
  assert.doesNotMatch(source, /waitForNewProfile|secret handoff deadline/);
  assert.match(makefile, /^\.PHONY: all native-build clean install packageinstall$/m);
  assert.match(makefile, /^all: native-build style\.css$/m);
  assert.match(makefile, /^style\.css: native-build$/m);
  assert.match(makefile, /^\s*\/usr\/local\/tool\/snpm install$/m);
  assert.match(makefile, /^\s*\/usr\/local\/tool\/snpm run build$/m);
});
