import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const appSource = await readFile(new URL("../src/App.vue", import.meta.url), "utf8");
const cssSource = await readFile(new URL("../src/styles/native.css", import.meta.url), "utf8");
const cssDist = await readFile(new URL("../dist/style.css", import.meta.url), "utf8");

function loadAppComponent() {
  const script = appSource.match(/<script>\s*([\s\S]*?)\s*<\/script>/);
  assert.ok(script, "App.vue script block is missing");
  let executable = script[1]
    .replace(/^import \{ ActionIcon \} from "\.\/ActionIcon";\s*/m, "")
    .replace(/^import \{ createAutosaveCoordinator \} from "\.\/autosave";\s*/m, "")
    .replace(/^import \{ installControlLayout \} from "\.\/controlLayout";\s*/m, "")
    .replace(/import \{[\s\S]*?\}\s*from "\.\/api";\s*/, "")
    .replace(/^import SecurityPanel from "\.\/SecurityPanel\.vue";\s*/m, "")
    .replace("export default {", "const AppComponent = {");
  executable += "\nObject.defineProperty(AppComponent, '__testSanitizedTroubleshootingText', { value: sanitizedTroubleshootingText });\nreturn AppComponent;";
  const stubs = {
    ACTIONS: {},
    AUTOSAVE_API_LIMITS: Object.freeze({}),
    MAX_RESPONSE_BYTES: 1024 * 1024,
    QueuedOutcomeUnknownError: class QueuedOutcomeUnknownError extends Error {},
    SNAPSHOT_SCHEMA: "sdsync.dsm-api.v1",
    apiGet: async () => ({}),
    apiPost: async () => ({}),
    purgeReconciliationAuth() {},
    reconcileMutationRequest: async () => ({}),
    arrayOf: (value) => Array.isArray(value) ? value : [],
    boundedText: (value, fallback = "") => String(typeof value === "string" ? value : fallback).slice(0, 65536),
    formatBytes: String,
    formatDate: (value) => Number(value) > 0 ? `date:${value}` : "Unavailable",
    formatDuration: String,
    numberOr: (value, fallback) => Number.isFinite(Number(value)) ? Number(value) : fallback,
    pick: (model, ...keys) => keys.map((key) => model && model[key]).find((value) => value !== undefined),
    createAutosaveCoordinator: () => ({}),
    installControlLayout: () => () => {},
    ActionIcon: { name: "ActionIcon" },
    SecurityPanel: {}
  };
  return Function(...Object.keys(stubs), executable)(...Object.values(stubs));
}

function bind(context, methods, names) {
  for (const name of names) context[name] = (...args) => methods[name].call(context, ...args);
  return context;
}

const component = loadAppComponent();
const methods = component.methods;

test("Activity and Logs expose per-record and filtered copy actions without native titles", () => {
  const templateStart = appSource.indexOf("<template>");
  const templateEnd = appSource.lastIndexOf("</template>", appSource.indexOf("<script>"));
  const template = appSource.slice(templateStart, templateEnd);
  for (const phrase of [
    "Copy all visible activity events",
    "Copy this event as bounded, sanitized troubleshooting text",
    "Copy all visible package logs",
    "Copy this log record as bounded, sanitized troubleshooting text"
  ]) assert.match(template, new RegExp(phrase));
  assert.match(template, /@click="copyActivityEvent\(event\)"/);
  assert.match(template, /@click="copyLogRecord\(record\)"/);
  assert.match(template, /@click="copyVisibleActivity"/);
  assert.match(template, /@click="copyVisibleLogs"/);
  assert.ok((template.match(/<action-icon name="copy"/g) || []).length >= 4);

  const copyButtons = [...template.matchAll(/<v-button\b(?=[^>]*(?:copyActivityEvent|copyLogRecord|copyVisibleActivity|copyVisibleLogs))[^>]*>/g)]
    .map((match) => match[0]);
  assert.equal(copyButtons.length, 4);
  for (const button of copyButtons) {
    assert.match(button, /\btooltip=/, "copy control needs the authored DSM tooltip");
    assert.match(button, /\baria-label=/, "copy control needs an accessible name");
    assert.doesNotMatch(button, /\btitle=/, "copy control must not add a duplicate native tooltip");
  }
});

test("copied activity and log evidence is structured, redacted, and bounded", () => {
  const activity = methods.activityEvidence.call({}, {
    epoch: 123,
    code: "authentication.failed",
    profile: "office",
    state: "failed",
    category: "authentication",
    level: "error",
    client_request_id: "a".repeat(32),
    message: "Authorization: Bearer top-secret\npassword=hunter2\nurl=https://user:private@nas.invalid/path?token=query-secret\njson={\"password\":\"json-password\",\"token\":\"json-token\",\"totp\":\"123456\"}"
  });
  assert.match(activity, /^Synology Drive Sync activity event/m);
  assert.match(activity, /Client request ID: a{32}/);
  assert.match(activity, /Authorization: \[redacted\]/i);
  assert.match(activity, /password=\[redacted\]/i);
  assert.match(activity, /https:\/\/\[redacted\]@nas\.invalid/);
  assert.match(activity, /token=\[redacted\]/i);
  assert.doesNotMatch(activity, /top-secret|hunter2|private|query-secret|json-password|json-token|123456/);
  assert.match(activity, /"password":\[redacted\]/i);
  assert.match(activity, /"token":\[redacted\]/i);
  assert.match(activity, /"totp":\[redacted\]/i);

  const log = methods.logEvidence.call({}, {
    source: "api",
    lineCount: 2,
    text: `Cookie: _SSID=session-secret\nsecret=another-secret\ndetail=${"z".repeat(70000)}`
  });
  assert.match(log, /^Synology Drive Sync package log/m);
  assert.match(log, /Cookie: \[redacted\]/i);
  assert.doesNotMatch(log, /session-secret/);
  assert.ok(log.length <= 64 * 1024);
  assert.match(log, /\[truncated: bounded troubleshooting copy\]$/);
});

test("actual Activity copy redacts complete API fields before display limits", async () => {
  const cutoffUserinfo = (secret, filler, safe) => `https://user:${secret}${"x".repeat(filler)}@nas.invalid/path?safe=${safe}`;
  const secrets = [
    "activity-code-cutoff-secret", "activity-profile-cutoff-secret",
    "activity-state-cutoff-secret", "activity-category-cutoff-secret",
    "activity-level-cutoff-secret", "activity-message-cutoff-secret"
  ];
  const rawEvent = {
    epoch: 123,
    code: cutoffUserinfo(secrets[0], 180, "retained-activity-code"),
    profile: cutoffUserinfo(secrets[1], 180, "retained-activity-profile"),
    state: cutoffUserinfo(secrets[2], 180, "retained-activity-state"),
    category: cutoffUserinfo(secrets[3], 180, "retained-activity-category"),
    level: cutoffUserinfo(secrets[4], 180, "retained-activity-level"),
    message: cutoffUserinfo(secrets[5], 2200, "retained-activity-message"),
    client_request_id: "a".repeat(32)
  };
  const normalized = component.computed.reversedActivity.call({
    activityEvents: [rawEvent],
    activitySearch: "",
    activityCategory: "all",
    activityLevel: "all"
  });
  assert.equal(normalized.length, 1);
  assert.ok(normalized[0].code.length <= 128);
  assert.ok(normalized[0].message.length <= 2048);

  const writes = [];
  const context = bind({
    async writeTroubleshootingClipboard(value) { writes.push(value); },
    toast() {}
  }, methods, ["copyTroubleshootingText", "activityEvidence", "copyActivityEvent"]);
  assert.equal(await context.copyActivityEvent(normalized[0]), true);
  assert.equal(writes.length, 1);
  for (const secret of secrets) {
    assert.doesNotMatch(normalized[0][secret === secrets[5] ? "message" : ["code", "profile", "state", "category", "level"][secrets.indexOf(secret)]], new RegExp(secret));
    assert.doesNotMatch(writes[0], new RegExp(secret), `actual Activity copy leaked ${secret}`);
  }
  assert.match(writes[0], /https:\/\/\[redacted\]@nas\.invalid/);
});

test("actual Log ingestion and copy sanitize before former field cutoffs", async () => {
  const exactLine = (prefix, length = 8192) => `${prefix}${"s".repeat(Math.max(0, length - prefix.length))}`.slice(0, length);
  const cutoffLine = (secret) => {
    const prefix = `url=https://user:${secret}`;
    return `${prefix}${"x".repeat(8190 - prefix.length)}@h`;
  };
  const cutoffUserinfo = (secret, length = 70000) => {
    const prefix = `https://user:${secret}`;
    return `${prefix}${"x".repeat(Math.max(1, length - prefix.length - 2))}@h`;
  };
  const secrets = [
    "realistic-multiline-cutoff-secret", "log-source-cutoff-secret",
    "log-timestamp-cutoff-secret", "log-message-cutoff-secret",
    "log-string-cutoff-secret"
  ];
  const safeLines = Array.from({ length: 7 }, (_, index) => exactLine(`safe-line-${index}:`));
  const largeSafeTail = "retained-safe-content-beyond-64k";
  const largeSafeLines = Array.from({ length: 9 }, (_, index) => exactLine(
    index === 8 ? largeSafeTail : `large-safe-line-${index}:`
  ));
  const records = methods.logRecordsFrom.call({}, {
    logs: [
      { source: "api", lines: [...safeLines, cutoffLine(secrets[0])] },
      { source: "controller", lines: largeSafeLines },
      { source: cutoffUserinfo(secrets[1]), message: "retained-source-message" },
      { source: "scheduler", timestamp: cutoffUserinfo(secrets[2]), message: "retained-timestamp-message" },
      { source: "bridge", message: cutoffUserinfo(secrets[3]) },
      cutoffUserinfo(secrets[4])
    ]
  });
  assert.equal(records.length, 6);
  assert.ok(records[1].text.length > 64 * 1024, "safe log display content was still capped at 64 KiB");
  assert.match(records[1].text, new RegExp(largeSafeTail));
  assert.match(records[0].text, /url=https:\/\/\[redacted\]@h/);

  const writes = [];
  const context = bind({
    logRecords: records,
    logSource: "all",
    logLines: 200,
    async writeTroubleshootingClipboard(value) { writes.push(value); },
    toast() {}
  }, methods, ["logEvidence", "copyTroubleshootingText", "copyVisibleLogs"]);
  assert.equal(await context.copyVisibleLogs(), true);
  assert.equal(writes.length, 1);
  for (const secret of secrets) {
    assert.doesNotMatch(records.map((record) => `${record.source}\n${record.text}`).join("\n"), new RegExp(secret));
    assert.doesNotMatch(writes[0], new RegExp(secret), `actual Log copy leaked ${secret}`);
  }
  for (const neighbor of [
    "retained-source-message", "retained-timestamp-message", "@h"
  ]) assert.match(writes[0], new RegExp(neighbor));
});

test("hostile DSM session, encoded URL, and escaped JSON credential shapes are redacted", () => {
  const secrets = [
    "raw-sid-secret", "upper-sid-secret", "ssid-secret", "session-id-secret",
    "api-key-secret", "json-cookie-secret", "json-cookie-ssid-secret",
    "encoded-password-secret", "encoded-session-secret", "encoded-userinfo-secret",
    "partly-encoded-userinfo-secret", "double-encoded-secret",
    "escaped-json-secret", "escaped-json-suffix-secret",
    "raw-syno-token-secret", "underscored-syno-token-secret",
    "json-syno-token-secret", "query-syno-token-secret",
    "escaped-syno-token-secret"
  ];
  const hostile = [
    `_sid=${secrets[0]}`,
    `SID: ${secrets[1]}`,
    `_SSID = ${secrets[2]}`,
    `session_id=${secrets[3]}`,
    `api_key: ${secrets[4]}`,
    `json={"cookie":"id=${secrets[5]}; _SSID=${secrets[6]}","safe":"retained"}`,
    `encoded=https%3A%2F%2Fnas.invalid%2Fwebapi%3Fpassword%3D${secrets[7]}%26session_id%3D${secrets[8]}`,
    `encoded_userinfo=https%3A%2F%2Fuser%3A${secrets[9]}%40nas.invalid%2Fwebapi`,
    `partly_encoded_userinfo=https://user%3A${secrets[10]}%40nas.invalid/webapi`,
    `double_encoded=%257B%2522api_key%2522%253A%2522${secrets[11]}%2522%252C%2522safe%2522%253Atrue%257D`,
    `escaped={\\"password\\":\\"${secrets[12]}\\\\\\"${secrets[13]}\\",\\"safe\\":true}`,
    `SynoToken=${secrets[14]}`,
    `syno_token: ${secrets[15]}`,
    `syno_json={"SynoToken":"${secrets[16]}","safe":"retained-too"}`,
    `syno_query=https://nas.invalid/webapi?syno_token=${secrets[17]}&safe=retained-query`,
    `syno_escaped={\\"syno_token\\":\\"${secrets[18]}\\",\\"safe\\":true}`
  ].join("\n");
  const evidence = methods.logEvidence.call({}, {
    source: "api",
    lineCount: hostile.split("\n").length,
    text: hostile
  });

  assert.ok((evidence.match(/\[redacted\]/g) || []).length >= 16, evidence);
  for (const secret of secrets) {
    assert.doesNotMatch(evidence, new RegExp(secret), `leaked hostile value ${secret}`);
  }
  assert.match(evidence, /"safe":"retained"/);
  assert.match(evidence, /"safe":"retained-too"/);
});

test("credential value delimiters cannot expose suffixes and raw fields fail closed to line end", () => {
  const secrets = [
    "raw-comma-head", "raw-comma-tail",
    "raw-semicolon-head", "raw-semicolon-tail",
    "raw-ampersand-head", "raw-ampersand-tail",
    "json-head", "json-comma-tail", "json-semicolon-tail", "json-ampersand-tail",
    "escaped-head", "escaped-comma-tail", "escaped-semicolon-tail", "escaped-ampersand-tail",
    "query-head", "query-comma-tail", "query-semicolon-tail", "query-ampersand-tail"
  ];
  const hostile = [
    `password=${secrets[0]},${secrets[1]};safe_raw_comma=retained-raw-comma`,
    `passwd=${secrets[2]};${secrets[3]},safe_raw_semicolon=retained-raw-semicolon`,
    `secret=${secrets[4]}&${secrets[5]}&safe_raw_ampersand=retained-raw-ampersand`,
    `json={"password":"${secrets[6]},${secrets[7]};${secrets[8]}&${secrets[9]}","safe_json":"retained-json"}`,
    `escaped={\\"password\\":\\"${secrets[10]},${secrets[11]};${secrets[12]}&${secrets[13]}\\",\\"safe_escaped\\":\\"retained-escaped\\"}`,
    `query=https://nas.invalid/webapi?syno_token=${secrets[14]},${secrets[15]};${secrets[16]}&${secrets[17]}&safe_query=retained-query-suffix`
  ].join("\n");
  const evidence = methods.logEvidence.call({}, {
    source: "api",
    lineCount: hostile.split("\n").length,
    text: hostile
  });

  for (const secret of secrets) {
    assert.doesNotMatch(evidence, new RegExp(secret), `leaked delimited credential segment ${secret}`);
  }
  for (const retained of ["retained-json", "retained-escaped"]) {
    assert.match(evidence, new RegExp(retained), `removed quoted safe neighboring field ${retained}`);
  }
  for (const ambiguous of [
    "retained-raw-comma", "retained-raw-semicolon", "retained-raw-ampersand", "retained-query-suffix"
  ]) assert.doesNotMatch(evidence, new RegExp(ambiguous), `preserved ambiguous same-line raw field ${ambiguous}`);
});

test("encoded quoted credentials honor escape depth and preserve neighboring safe fields", () => {
  const secrets = [
    "encoded-single-head", "encoded-single-tail",
    "encoded-double-head", "encoded-double-tail",
    "encoded-triple-head", "encoded-triple-tail"
  ];
  const hostile = [
    `single=%7B%22password%22%3A%22${secrets[0]}%5C%22${secrets[1]}%22%2C%22safe_single%22%3A%22retained-encoded-single%22%7D`,
    `double=%257B%2522password%2522%253A%2522${secrets[2]}%255C%2522${secrets[3]}%2522%252C%2522safe_double%2522%253A%2522retained-encoded-double%2522%257D`,
    `triple=%25257B%252522SynoToken%252522%25253A%252522${secrets[4]}%25255C%252522${secrets[5]}%252522%25252C%252522safe_triple%252522%25253A%252522retained-encoded-triple%252522%25257D`
  ].join("\n");
  const evidence = methods.logEvidence.call({}, {
    source: "api",
    lineCount: hostile.split("\n").length,
    text: hostile
  });

  for (const secret of secrets) {
    assert.doesNotMatch(evidence, new RegExp(secret), `leaked encoded quoted credential segment ${secret}`);
  }
  for (const retained of [
    "retained-encoded-single", "retained-encoded-double", "retained-encoded-triple"
  ]) assert.match(evidence, new RegExp(retained), `removed encoded safe neighboring field ${retained}`);
});

test("unquoted credential values do not leak bracket or brace suffixes", () => {
  const secrets = [
    "bracket-head", "bracket-tail", "brace-head", "brace-tail",
    "query-bracket-head", "query-bracket-tail"
  ];
  const hostile = [
    `password=${secrets[0]}]${secrets[1]};safe_bracket=retained-bracket`,
    `SynoToken=${secrets[2]}}${secrets[3]}&safe_brace=retained-brace`,
    `query=https://nas.invalid/webapi?syno_token=${secrets[4]}]${secrets[5]}&safe_query_bracket=retained-query-bracket`
  ].join("\n");
  const evidence = methods.logEvidence.call({}, {
    source: "api",
    lineCount: hostile.split("\n").length,
    text: hostile
  });

  for (const secret of secrets) {
    assert.doesNotMatch(evidence, new RegExp(secret), `leaked bracketed credential segment ${secret}`);
  }
  for (const ambiguous of ["retained-bracket", "retained-brace", "retained-query-bracket"]) {
    assert.doesNotMatch(evidence, new RegExp(ambiguous), `preserved ambiguous bracketed raw field ${ambiguous}`);
  }
});

test("app-specific DSM credentials redact without hiding expiry or source metadata", () => {
  const secrets = [
    "header-csrf-secret", "raw-proof-secret", "raw-otp-secret",
    "raw-csrf-header-secret", "raw-csrf-key-secret", "raw-csrf-secret",
    "json-csrf-secret", "json-proof-secret", "json-otp-secret",
    "encoded-otp-secret", "encoded-proof-secret", "encoded-csrf-secret",
    "env-password-secret", "env-otp-secret", "env-totp-secret", "env-remote-log-secret",
    "http-cookie-secret", "http-syno-token-secret", "http-sdsync-csrf-secret"
  ];
  const hostile = [
    `X-SDSYNC-CSRF: ${secrets[0]}`,
    `connection_proof=${secrets[1]}`,
    "connection_proof_expires_at_epoch=retained-proof-expiry",
    `otp_code=${secrets[2]}`,
    "otp_code_source=retained-otp-code-source",
    `csrf_header=${secrets[3]}`,
    "password_source=retained-password-source",
    `csrf_key=${secrets[4]}`,
    "totp_source=retained-totp-source",
    `csrf=${secrets[5]}`,
    "csrf_lifetime_seconds=retained-csrf-lifetime",
    `json={"X-SDSYNC-CSRF":"${secrets[6]}","connection_proof":"${secrets[7]}","otp_code":"${secrets[8]}","credential_kind":"retained-credential-kind","credential_source":"retained-credential-source","source":"retained-source"}`,
    `single=%7B%27otp_code%27%3A%27${secrets[9]}%27%2C%27source%27%3A%27retained-single-quoted-source%27%7D`,
    `double=%257B%2527connection_proof%2527%253A%2527${secrets[10]}%2527%252C%2527connection_proof_expires_at_epoch%2527%253A123%257D`,
    `triple=%25257B%252527X-SDSYNC-CSRF%252527%25253A%252527${secrets[11]}%252527%25252C%252527source%252527%25253A%252527retained-triple-quoted-source%252527%25257D`,
    `SDSYNC_PASSWORD=${secrets[12]}`,
    `SDSYNC_OTP=${secrets[13]}`,
    `SDSYNC_TOTP=${secrets[14]}`,
    `SDSYNC_REMOTE_LOG_TOKEN=${secrets[15]}`,
    `HTTP_COOKIE=${secrets[16]}`,
    `HTTP_X_SYNO_TOKEN=${secrets[17]}`,
    `HTTP_X_SDSYNC_CSRF=${secrets[18]}`,
    "SDSYNC_PASSWORD_FILE=retained-password-file",
    "SDSYNC_OTP_SOURCE=retained-otp-source",
    "SDSYNC_TOTP_FILE=retained-totp-file",
    "SDSYNC_REMOTE_LOG_TOKEN_SOURCE=retained-remote-log-source",
    "HTTP_COOKIE_SOURCE=retained-http-cookie-source",
    "HTTP_X_SYNO_TOKEN_SOURCE=retained-http-syno-source",
    "HTTP_X_SDSYNC_CSRF_SOURCE=retained-http-csrf-source"
  ].join("\n");
  const evidence = methods.logEvidence.call({}, {
    source: "api",
    lineCount: hostile.split("\n").length,
    text: hostile
  });

  for (const secret of secrets) {
    assert.doesNotMatch(evidence, new RegExp(secret), `leaked app-specific DSM credential ${secret}`);
  }
  for (const retained of [
    "retained-proof-expiry", "retained-otp-code-source", "retained-password-source",
    "retained-totp-source", "retained-csrf-lifetime", "retained-credential-kind",
    "retained-credential-source", "retained-source", "retained-single-quoted-source",
    "connection_proof_expires_at_epoch%2527%253A123", "retained-triple-quoted-source",
    "retained-password-file", "retained-otp-source", "retained-totp-file",
    "retained-remote-log-source", "retained-http-cookie-source", "retained-http-syno-source",
    "retained-http-csrf-source"
  ]) assert.match(evidence, new RegExp(retained), `false-matched safe app metadata ${retained}`);
});

test("pretty multiline credential values redact through leading and continuation lines", () => {
  const secrets = ["multiline-token-sentinel", "multiline-proof-sentinel", "multiline-continuation-sentinel"];
  const hostile = [
    "{",
    "  \"SynoToken\":",
    `    \"${secrets[0]}\",`,
    "  \"safe_pretty\": \"retained-pretty-neighbor\"",
    "}",
    "connection_proof=",
    `  ${secrets[1]}`,
    `  ${secrets[2]}`,
    "safe_after_multiline=retained-after-multiline"
  ].join("\n");
  const evidence = methods.logEvidence.call({}, {
    source: "api",
    lineCount: hostile.split("\n").length,
    text: hostile
  });

  for (const secret of secrets) assert.doesNotMatch(evidence, new RegExp(secret));
  assert.match(evidence, /retained-pretty-neighbor/);
  assert.match(evidence, /retained-after-multiline/);
});

test("percent-encoded escaped JSON credentials redact at every supported depth", () => {
  const secrets = [
    "escaped-encoded-single-head", "escaped-encoded-single-tail",
    "escaped-encoded-double-head", "escaped-encoded-double-tail",
    "escaped-encoded-triple-head", "escaped-encoded-triple-tail"
  ];
  const hostile = [
    `single=%22%7B%5C%22SynoToken%5C%22%3A%5C%22${secrets[0]}%5C%5C%5C%22${secrets[1]}%5C%22%2C%5C%22safe_single%5C%22%3A%5C%22retained-escaped-encoded-single%5C%22%7D%22`,
    `double=%2522%257B%255C%2522SynoToken%255C%2522%253A%255C%2522${secrets[2]}%255C%255C%255C%2522${secrets[3]}%255C%2522%252C%255C%2522safe_double%255C%2522%253A%255C%2522retained-escaped-encoded-double%255C%2522%257D%2522`,
    `triple=%252522%25257B%25255C%252522SynoToken%25255C%252522%25253A%25255C%252522${secrets[4]}%25255C%25255C%25255C%252522${secrets[5]}%25255C%252522%25252C%25255C%252522safe_triple%25255C%252522%25253A%25255C%252522retained-escaped-encoded-triple%25255C%252522%25257D%252522`
  ].join("\n");
  const evidence = methods.logEvidence.call({}, {
    source: "api",
    lineCount: hostile.split("\n").length,
    text: hostile
  });

  for (const secret of secrets) assert.doesNotMatch(evidence, new RegExp(secret));
  for (const retained of [
    "retained-escaped-encoded-single", "retained-escaped-encoded-double", "retained-escaped-encoded-triple"
  ]) assert.match(evidence, new RegExp(retained));
});

test("mixed-encoded and protocol-relative URL userinfo is removed", () => {
  const secrets = ["mixed-userinfo-sentinel", "relative-userinfo-sentinel"];
  const hostile = [
    `mixed=https://user:${secrets[0]}%40nas.invalid/path?safe=retained-mixed-userinfo`,
    `relative=//user:${secrets[1]}@nas.invalid/path?safe=retained-relative-userinfo`
  ].join("\n");
  const evidence = methods.logEvidence.call({}, {
    source: "api",
    lineCount: 2,
    text: hostile
  });

  for (const secret of secrets) assert.doesNotMatch(evidence, new RegExp(secret));
  assert.match(evidence, /https:\/\/\[redacted\]@nas\.invalid\/path\?safe=retained-mixed-userinfo/);
  assert.match(evidence, /\/\/\[redacted\]@nas\.invalid\/path\?safe=retained-relative-userinfo/);
});

test("URL userinfo redacts every mixed delimiter depth and selects the final at delimiter", () => {
  const delimiter = (raw, hex, depth) => depth === 0 ? raw : `%${"25".repeat(depth - 1)}${hex}`;
  const secrets = [];
  const retained = [];
  const urls = [];
  for (let schemeDepth = 0; schemeDepth <= 3; schemeDepth += 1) {
    for (let slashDepth = 0; slashDepth <= 3; slashDepth += 1) {
      for (let credentialDepth = 0; credentialDepth <= 3; credentialDepth += 1) {
        for (let atDepth = 0; atDepth <= 3; atDepth += 1) {
          const id = `s${schemeDepth}-l${slashDepth}-c${credentialDepth}-a${atDepth}`;
          const secret = `mixed-url-${id}-head-sentinel-tail-sentinel`;
          const safe = `retained-mixed-url-${id}`;
          const schemeColon = delimiter(":", "3A", schemeDepth);
          const slash = delimiter("/", "2F", slashDepth);
          const credentialColon = delimiter(":", "3A", credentialDepth);
          const at = delimiter("@", "40", atDepth);
          secrets.push(secret);
          retained.push(safe);
          urls.push(`https${schemeColon}${slash}${slash}user${credentialColon}${secret}${at}nas.invalid/path?safe=${safe}`);
        }
      }
    }
  }
  for (const [prefix, firstAt, finalAt, id] of [
    ["https://", "@", "@", "raw-last-at"],
    ["https://", "%40", "@", "mixed-last-at"],
    ["https%3A%2F%2F", "%40", "%40", "encoded-last-at"],
    ["//", "@", "%40", "relative-last-at"]
  ]) {
    const secret = `${id}-head-sentinel${firstAt}${id}-tail-sentinel`;
    const safe = `retained-${id}`;
    secrets.push(`${id}-head-sentinel`, `${id}-tail-sentinel`);
    retained.push(safe);
    urls.push(`${prefix}user:${secret}${finalAt}nas.invalid/path?safe=${safe}`);
  }
  const evidence = component.__testSanitizedTroubleshootingText(urls.join("\n"), 256 * 1024);

  assert.equal(urls.length, 260);
  for (const secret of secrets) assert.doesNotMatch(evidence, new RegExp(secret), `leaked mixed URL userinfo ${secret}`);
  for (const safe of retained) assert.match(evidence, new RegExp(safe), `removed URL host/query neighbor ${safe}`);
});

test("URL userinfo near-misses remain linear", () => {
  const target = 1024 * 1024;
  const source = `https:%2F%2Fhost-without-userinfo-${"x".repeat(target)}`;
  const started = process.hrtime.bigint();
  const evidence = component.__testSanitizedTroubleshootingText(source, 256 * 1024);
  const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;
  assert.ok(evidence.length <= 256 * 1024);
  assert.ok(elapsedMs < 750, `mixed URL near-miss sanitization took ${elapsedMs.toFixed(1)}ms`);
});

test("odd encoded escape wrappers redact across key and punctuation combinations", () => {
  const encodedToken = (hex, depth) => `%${"25".repeat(depth - 1)}${hex}`;
  const secrets = [];
  const retained = [];
  const cases = [];
  for (let encodingDepth = 1; encodingDepth <= 3; encodingDepth += 1) {
    const backslash = encodedToken("5C", encodingDepth);
    const openBrace = encodedToken("7B", encodingDepth);
    const closeBrace = encodedToken("7D", encodingDepth);
    const comma = encodedToken("2C", encodingDepth);
    for (let escapeDepth = 1; escapeDepth <= 3; escapeDepth += 1) {
      const wrapperBackslashes = backslash.repeat((escapeDepth * 2) - 1);
      for (const [quoteName, quoteHex] of [["double", "22"], ["single", "27"]]) {
        const wrapper = `${wrapperBackslashes}${encodedToken(quoteHex, encodingDepth)}`;
        for (const [assignmentName, assignmentHex] of [["colon", "3A"], ["equals", "3D"]]) {
          const assignment = encodedToken(assignmentHex, encodingDepth);
          for (const key of ["SynoToken", "connection_proof", "otp_code"]) {
            const id = `e${encodingDepth}-x${escapeDepth}-${quoteName}-${assignmentName}-${key.toLowerCase()}`;
            const secret = `wrapped-${id}-head-sentinel-tail-sentinel`;
            const safe = `retained-${id}-neighbor`;
            secrets.push(secret);
            retained.push(safe);
            cases.push(`${id}=${openBrace}${wrapper}${key}${wrapper}${assignment}${wrapper}${secret}${wrapper}${comma}${wrapper}safe_field${wrapper}${assignment}${wrapper}${safe}${wrapper}${closeBrace}`);
          }
        }
      }
    }
  }
  const hostile = cases.join("\n");
  const evidence = methods.logEvidence.call({}, {
    source: "api",
    lineCount: cases.length,
    text: hostile
  });

  assert.equal(cases.length, 108);
  for (const secret of secrets) assert.doesNotMatch(evidence, new RegExp(secret), `leaked odd-wrapper credential ${secret}`);
  for (const safe of retained) assert.match(evidence, new RegExp(safe), `removed odd-wrapper safe neighbor ${safe}`);
});

test("encoded wrapper no-match runs remain bounded at the maximum visible-copy size", () => {
  const maximum = 256 * 1024;
  const hostile = `${"%5c".repeat(87380)}X`;
  const started = process.hrtime.bigint();
  const evidence = component.__testSanitizedTroubleshootingText(hostile, maximum);
  const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;

  assert.ok(evidence.length <= maximum);
  assert.match(evidence, /X$/);
  assert.ok(elapsedMs < 750, `maximum-size wrapper no-match sanitization took ${elapsedMs.toFixed(1)}ms`);

  const aggregateStarted = process.hrtime.bigint();
  const aggregate = component.__testSanitizedTroubleshootingText(hostile.repeat(4), maximum);
  const aggregateElapsedMs = Number(process.hrtime.bigint() - aggregateStarted) / 1e6;
  assert.ok(aggregate.length <= maximum);
  assert.match(aggregate, /\[truncated: bounded troubleshooting copy\]$/);
  assert.ok(aggregateElapsedMs < 750, `aggregate wrapper no-match sanitization took ${aggregateElapsedMs.toFixed(1)}ms`);
});

test("dense same-line credential aggregates remain near-linear", () => {
  const repeated = (fragment, target) => fragment.repeat(Math.ceil(target / fragment.length)).slice(0, target);
  const cases = [
    ["unquoted-256KiB", repeated("password=abcdefghijklmnopqrstuvwxyz&safe=x;", 256 * 1024), "abcdefghijklmnopqrstuvwxyz"],
    ["unquoted-1MiB", repeated("password=abcdefghijklmnopqrstuvwxyz&safe=x;", 1024 * 1024), "abcdefghijklmnopqrstuvwxyz"],
    ["quoted-1MiB", repeated('{"password":"quoted-dense-sentinel","safe":"x"};', 1024 * 1024), "quoted-dense-sentinel"],
    ["already-redacted-1MiB", repeated('{"password":"[redacted]","safe":"x"};', 1024 * 1024), null]
  ];
  for (const [label, source, secret] of cases) {
    const started = process.hrtime.bigint();
    const evidence = component.__testSanitizedTroubleshootingText(source, 256 * 1024);
    const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;
    assert.ok(evidence.length <= 256 * 1024);
    if (secret) assert.doesNotMatch(evidence, new RegExp(secret), `${label} leaked its dense credential`);
    assert.ok(elapsedMs < 750, `${label} sanitization took ${elapsedMs.toFixed(1)}ms`);
  }
});

test("credential terminators beyond the output cutoff cannot expose a partial boundary line", () => {
  const limit = 256;
  const safePrefix = "safe_record=retained-before-boundary\n";
  const cases = [
    {
      label: "raw userinfo",
      secret: "raw-cutoff-userinfo-sentinel",
      lead: "url=https://user:",
      terminator: "@nas.invalid/path?safe=retained-after-raw-userinfo"
    },
    {
      label: "encoded userinfo delimiter",
      secret: "encoded-cutoff-userinfo-sentinel",
      lead: "url=https://user:",
      terminator: "%40nas.invalid/path?safe=retained-after-encoded-userinfo"
    },
    {
      label: "contextual DSM cookie id",
      secret: "cookie-cutoff-id-sentinel",
      lead: "  id=",
      terminator: "; stay_login=1; safe_cookie=retained-after-cookie"
    }
  ];

  for (const boundaryCase of cases) {
    const partial = `${safePrefix}${boundaryCase.lead}${boundaryCase.secret}`;
    const filler = "x".repeat(Math.max(1, limit - partial.length + 16));
    const source = `${partial}${filler}${boundaryCase.terminator}${"y".repeat(limit)}`;
    const terminatorOffset = source.indexOf(boundaryCase.terminator);
    assert.ok(terminatorOffset > limit, `${boundaryCase.label} terminator must exercise the post-cutoff context`);

    const evidence = component.__testSanitizedTroubleshootingText(source, limit);
    assert.doesNotMatch(evidence, new RegExp(boundaryCase.secret), `${boundaryCase.label} leaked before its terminator`);
    assert.match(evidence, /retained-before-boundary/);
    assert.match(evidence, /\[truncated: bounded troubleshooting copy\]$/);
    assert.ok(evidence.length <= limit);
  }
});

test("exact serialized credentials and contextual DSM session cookie ids redact without metadata false matches", () => {
  const secrets = [
    "raw-authorization-sentinel", "json-authorization-sentinel",
    "raw-secret-value-sentinel", "json-secret-value-sentinel",
    "raw-totp-secret-sentinel", "json-totp-secret-sentinel",
    "forward-cookie-id-sentinel", "reverse-cookie-id-sentinel"
  ];
  const hostile = [
    `authorization=${secrets[0]}`,
    "authorization_source=retained-authorization-source",
    `secret_value=${secrets[2]}`,
    "secret_value_source=retained-secret-value-source",
    `totp_secret=${secrets[4]}`,
    "totp_secret_file=retained-totp-secret-file",
    `json={"authorization":"${secrets[1]}","secret_value":"${secrets[3]}","totp_secret":"${secrets[5]}","authorization_scheme":"retained-authorization-scheme"}`,
    `  id=${secrets[6]}; stay_login=1; safe_cookie=retained-forward-cookie-neighbor`,
    `\tstay_login=1; id=${secrets[7]}; safe_cookie=retained-reverse-cookie-neighbor`,
    "id=retained-ordinary-record-id; record_type=activity",
    "  id=retained-noncookie-id; stay_login_state=1"
  ].join("\n");
  const evidence = methods.logEvidence.call({}, {
    source: "api",
    lineCount: hostile.split("\n").length,
    text: hostile
  });

  for (const secret of secrets) assert.doesNotMatch(evidence, new RegExp(secret), `leaked exact credential ${secret}`);
  for (const safe of [
    "retained-authorization-source", "retained-secret-value-source", "retained-totp-secret-file",
    "retained-authorization-scheme", "retained-forward-cookie-neighbor", "retained-reverse-cookie-neighbor",
    "retained-ordinary-record-id", "retained-noncookie-id"
  ]) assert.match(evidence, new RegExp(safe), `false-matched metadata or ordinary id ${safe}`);
});

test("proxy authorization and CGI authorization keys redact as whole fields", () => {
  const secrets = [
    "proxy-underscore-sentinel", "proxy-hyphen-json-sentinel",
    "http-authorization-sentinel", "http-proxy-authorization-sentinel"
  ];
  const hostile = [
    `proxy_authorization=${secrets[0]}`,
    `json={"proxy-authorization":"${secrets[1]}"}`,
    `HTTP_AUTHORIZATION=${secrets[2]}`,
    `HTTP_PROXY_AUTHORIZATION=${secrets[3]}`,
    "proxy_authorization_source=retained-proxy-source",
    "proxy-authorization-scheme=retained-proxy-scheme",
    "HTTP_AUTHORIZATION_SOURCE=retained-http-authorization-source",
    "HTTP_PROXY_AUTHORIZATION_SCHEME=retained-http-proxy-scheme"
  ].join("\n");
  const evidence = component.__testSanitizedTroubleshootingText(hostile, 64 * 1024);
  for (const secret of secrets) assert.doesNotMatch(evidence, new RegExp(secret));
  for (const safe of [
    "retained-proxy-source", "retained-proxy-scheme",
    "retained-http-authorization-source", "retained-http-proxy-scheme"
  ]) assert.match(evidence, new RegExp(safe));
});

test("encoded ASCII structural boundaries mirror raw key boundaries at every depth", () => {
  const secrets = [];
  const retained = [];
  const lines = [];
  for (let depth = 1; depth <= 3; depth += 1) {
    const encoded = (hex) => `%${"25".repeat(depth - 1)}${hex}`;
    for (let byte = 0; byte <= 0x7f; byte += 1) {
      const hex = byte.toString(16).padStart(2, "0").toUpperCase();
      const character = String.fromCharCode(byte);
      const structural = !/[A-Za-z0-9_-]/.test(character);
      const marker = `${structural ? "boundary" : "nonboundary"}-d${depth}-${hex}-sentinel`;
      lines.push(`case_d${depth}_${hex}=prefix${encoded(hex)}password${encoded("3D")}${marker}`);
      (structural ? secrets : retained).push(marker);
    }
  }
  const exactSecrets = ["encoded-colon-env-secret", "encoded-equals-env-secret", "encoded-path-secret"];
  lines.push(
    `env%3ASDSYNC_PASSWORD%3D${exactSecrets[0]}`,
    `env%3DSDSYNC_PASSWORD%3D${exactSecrets[1]}`,
    `url=https%3A%2F%2Fnas.invalid%2Fpassword%3D${exactSecrets[2]}%26safe%3Dok`
  );
  const evidence = component.__testSanitizedTroubleshootingText(lines.join("\n"), 256 * 1024);
  for (const secret of [...secrets, ...exactSecrets]) assert.doesNotMatch(evidence, new RegExp(secret));
  for (const safe of retained) assert.match(evidence, new RegExp(safe), `false-matched encoded key continuation ${safe}`);
});

test("encoded DSM id cookies redact across independently mixed delimiter depths", () => {
  const delimiter = (raw, hex, depth) => depth === 0 ? raw : `%${"25".repeat(depth - 1)}${hex}`;
  const secrets = [];
  const retained = [];
  const lines = [];
  for (let idEqualsDepth = 0; idEqualsDepth <= 3; idEqualsDepth += 1) {
    for (let semicolonDepth = 0; semicolonDepth <= 3; semicolonDepth += 1) {
      for (let spaceDepth = 0; spaceDepth <= 3; spaceDepth += 1) {
        for (let stayEqualsDepth = 0; stayEqualsDepth <= 3; stayEqualsDepth += 1) {
          for (const order of ["forward", "reverse"]) {
            const id = `i${idEqualsDepth}-s${semicolonDepth}-w${spaceDepth}-l${stayEqualsDepth}-${order}`;
            const secret = `mixed-cookie-${id}-sentinel`;
            const safe = `retained-mixed-cookie-${id}`;
            const idEquals = delimiter("=", "3D", idEqualsDepth);
            const stayEquals = delimiter("=", "3D", stayEqualsDepth);
            const semicolon = delimiter(";", "3B", semicolonDepth);
            const space = delimiter(" ", "20", spaceDepth);
            secrets.push(secret);
            retained.push(safe);
            lines.push(order === "forward"
              ? `id${idEquals}${secret}${semicolon}${space}stay_login${stayEquals}1${semicolon}${space}safe=${safe}`
              : `stay_login${stayEquals}1${semicolon}${space}id${idEquals}${secret}${semicolon}${space}safe=${safe}`);
          }
        }
      }
    }
  }
  for (let depth = 0; depth <= 3; depth += 1) lines.push(
    `id${delimiter("=", "3D", depth)}retained-ordinary-encoded-id-d${depth}; stay_login_state=1`
  );
  const evidence = component.__testSanitizedTroubleshootingText(lines.join("\n"), 256 * 1024);
  assert.equal(secrets.length, 512);
  for (const secret of secrets) assert.doesNotMatch(evidence, new RegExp(secret));
  for (const safe of retained) assert.match(evidence, new RegExp(safe));
  for (let depth = 0; depth <= 3; depth += 1) assert.match(evidence, new RegExp(`retained-ordinary-encoded-id-d${depth}`));

  const nearMiss = "id%25253Dordinary-value%25253B%252520stay_login_state%25253D1".repeat(16000);
  const started = process.hrtime.bigint();
  component.__testSanitizedTroubleshootingText(nearMiss, 256 * 1024);
  const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;
  assert.ok(elapsedMs < 750, `encoded cookie near-miss sanitization took ${elapsedMs.toFixed(1)}ms`);
});

function cssRuleAfter(css, selector, offset = 0) {
  const start = css.indexOf(selector, offset);
  assert.notEqual(start, -1, `missing ${selector}`);
  const open = css.indexOf("{", start);
  const close = css.indexOf("}", open);
  assert.ok(open > start && close > open, `missing declarations for ${selector}`);
  return css.slice(open + 1, close);
}

test("activity copy controls retain a bounded grid position above and below the 720px cliff", () => {
  for (const [label, css] of [["source", cssSource], ["dist", cssDist]]) {
    const mediumSelector = ".sdsync-app.sdsync-medium-shell:not(.sdsync-compact-shell) .sdsync-activity-feed li";
    assert.match(cssRuleAfter(css, mediumSelector), /grid-template-columns:\s*minmax\(112px, 0\.72fr\) minmax\(0, 1\.4fr\)/, label);
    const mediumCopy = cssRuleAfter(css, ".sdsync-app.sdsync-medium-shell:not(.sdsync-compact-shell) .sdsync-activity-feed .sdsync-evidence-copy");
    assert.match(mediumCopy, /grid-column:\s*2/);
    assert.match(mediumCopy, /grid-row:\s*2/);
    assert.match(mediumCopy, /justify-self:\s*end/);

    const viewportStart = css.indexOf("@media (max-width: 980px)");
    const compactStart = css.indexOf("@media (max-width: 720px)", viewportStart);
    assert.ok(viewportStart >= 0 && compactStart > viewportStart, `${label} responsive breakpoints are missing or reordered`);
    const mediumViewport = css.slice(viewportStart, compactStart);
    assert.match(mediumViewport, /\.sdsync-activity-feed li\s*\{[^}]*grid-template-columns:\s*minmax\(112px, 0\.72fr\) minmax\(0, 1\.4fr\)/s);
    assert.match(mediumViewport, /\.sdsync-activity-feed \.sdsync-evidence-copy\s*\{[^}]*grid-column:\s*2[^}]*grid-row:\s*2[^}]*justify-self:\s*end/s);

    const compactViewport = css.slice(compactStart);
    assert.match(compactViewport, /\.sdsync-activity-feed li\s*\{[^}]*grid-template-columns:\s*1fr/s);
    assert.match(compactViewport, /\.sdsync-activity-feed \.sdsync-evidence-copy\s*\{[^}]*grid-column:\s*1[^}]*grid-row:\s*auto[^}]*justify-self:\s*start/s);
  }
});

test("log records preserve source boundaries and the selected response byte ceiling", () => {
  const context = {};
  const records = methods.logRecordsFrom.call(context, {
    logs: [
      { source: "api", lines: ["first", "second"] },
      { source: "controller", lines: ["third"] }
    ]
  });
  assert.deepEqual(records.map((record) => [record.source, record.lineCount]), [["api", 2], ["controller", 1]]);
  assert.equal(records[0].text, "first\nsecond");
  assert.notEqual(records[0].id, records[1].id);
});

test("copy uses the secure Clipboard API and reports bounded redaction guidance", async () => {
  const priorWindow = globalThis.window;
  const priorDocument = globalThis.document;
  const writes = [];
  globalThis.window = { navigator: { clipboard: { async writeText(value) { writes.push(value); } } } };
  globalThis.document = {};
  const context = bind({
    toasts: [],
    toast(title, message, error = false) { this.toasts.push({ title, message, error }); }
  }, methods, ["writeTroubleshootingClipboard", "copyTroubleshootingText", "activityEvidence", "copyActivityEvent"]);
  try {
    const copied = await context.copyActivityEvent({
      epoch: 123, code: "alert.failed", profile: "office", state: "failed",
      category: "notifications", level: "error", message: "token=never-copy-this"
    });
    assert.equal(copied, true);
    assert.equal(writes.length, 1);
    assert.match(writes[0], /token=\[redacted\]/i);
    assert.doesNotMatch(writes[0], /never-copy-this/);
    assert.equal(context.toasts.at(-1).title, "Activity event copied");
    assert.equal(context.toasts.at(-1).error, false);
    assert.match(context.toasts.at(-1).message, /Known DSM session and credential field shapes were redacted/);
    assert.match(context.toasts.at(-1).message, /Review the bounded text before sharing/);
  } finally {
    globalThis.window = priorWindow;
    globalThis.document = priorDocument;
  }
});

test("copy falls back for DSM browsers and reports a rejected clipboard without leaking evidence", async () => {
  const priorWindow = globalThis.window;
  const priorDocument = globalThis.document;
  let appended = null;
  let restoredFocus = false;
  const body = {
    appendChild(node) { appended = node; node.parentNode = this; },
    removeChild(node) { if (appended === node) appended = null; node.parentNode = null; }
  };
  const documentMock = {
    body,
    activeElement: { focus() { restoredFocus = true; } },
    createElement() {
      return {
        value: "", style: {}, parentNode: null,
        setAttribute() {}, focus() {}, select() {}, setSelectionRange() {}
      };
    },
    execCommand(command) { return command === "copy"; }
  };
  globalThis.window = { navigator: { clipboard: { async writeText() { throw new Error("denied"); } } } };
  globalThis.document = documentMock;
  const context = bind({
    toasts: [],
    toast(title, message, error = false) { this.toasts.push({ title, message, error }); }
  }, methods, ["writeTroubleshootingClipboard", "copyTroubleshootingText"]);
  try {
    assert.equal(await context.copyTroubleshootingText("safe evidence", "Log"), true);
    assert.equal(appended, null, "temporary clipboard textarea must be removed");
    assert.equal(restoredFocus, true);

    documentMock.execCommand = () => false;
    assert.equal(await context.copyTroubleshootingText("do-not-echo", "Log"), false);
    assert.equal(context.toasts.at(-1).title, "Copy failed");
    assert.equal(context.toasts.at(-1).error, true);
    assert.doesNotMatch(context.toasts.at(-1).message, /do-not-echo/);
    assert.equal(appended, null, "failed fallback must still remove its textarea");
  } finally {
    globalThis.window = priorWindow;
    globalThis.document = priorDocument;
  }
});
