#!/usr/bin/env python3
"""Focused contract tests for the offline DSM desktop application and SPK UI."""

from __future__ import annotations

import hashlib
import io
import json
import math
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest
import zlib
from pathlib import Path


HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import build_spk  # noqa: E402
import validate_spk  # noqa: E402


UI = HERE / "package/ui"
UI_SOURCE = HERE / "ui-src"


class DsmUiContractTests(unittest.TestCase):
    def test_source_validator_covers_rootless_ui_and_dsm_bounds(self) -> None:
        validate_spk.validate_source()
        info = (HERE / "INFO.template").read_text(encoding="utf-8")
        self.assertIn('os_min_ver="7.0-40759"', info)
        self.assertIn('os_max_ver="7.4-99999"', info)
        self.assertIn('dsmuidir="ui"', info)
        self.assertIn('dsmappname="SYNO.SDS.App.SynologyDriveSync.Instance"', info)
        self.assertFalse(hasattr(validate_spk, "validate_ui_static"))
        for legacy in ("config", "app.js", "app.css"):
            self.assertFalse(UI.joinpath(legacy).exists())
        launcher = (UI / "index.html").read_bytes()
        validate_spk.validate_direct_launcher(launcher)
        self.assertEqual(
            hashlib.sha256(launcher).hexdigest(),
            build_spk.DIRECT_LAUNCHER_SHA256,
        )

    def test_direct_package_routes_open_the_native_dsm_appwindow(self) -> None:
        launcher = (UI / "index.html").read_bytes()
        document = launcher.decode("utf-8")
        target = "/webman/index.cgi?launchApp=SYNO.SDS.App.SynologyDriveSync.Instance"
        self.assertEqual(build_spk.DSM_LAUNCH_TARGET, target)
        self.assertIn(f'content="0;url={target}"', document)
        self.assertIn(f'<a href="{target}">Open Synology Drive Sync</a>', document)
        self.assertIn('name="referrer" content="no-referrer"', document)
        self.assertIn("default-src 'none'", document)
        self.assertNotRegex(document, r"<(?:script|style|img|iframe|object|embed|form)\b")

        # DSM's dsmuidir maps this installed index to both the directory-index
        # route and its explicit filename. The launcher then enters the same
        # native AppWindow class declared by dsmappname/app.config.
        info = validate_spk.parse_info((HERE / "INFO.template").read_bytes())
        self.assertEqual(info["dsmuidir"], "ui")
        self.assertEqual(info["dsmappname"], build_spk.DSM_APP_CLASS)
        self.assertEqual(
            {
                "/webman/3rdparty/synology-drive-sync/",
                "/webman/3rdparty/synology-drive-sync/index.html",
            },
            {
                f"/webman/3rdparty/{info['package']}/",
                f"/webman/3rdparty/{info['package']}/index.html",
            },
        )
        validate_spk.validate_direct_launcher(launcher)
        for tampered in (
            launcher.replace(target.encode(), b"https://evil.invalid/", 1),
            launcher.replace(b"<main>", b"<script>alert(1)</script><main>", 1),
            launcher.replace(b"no-referrer", b"unsafe-url", 1),
        ):
            with self.assertRaisesRegex(
                validate_spk.ValidationError,
                "exact reviewed native AppWindow redirect",
            ):
                validate_spk.validate_direct_launcher(tampered)

    def test_native_dsm_application_and_direct_notification_contract_match(self) -> None:
        config = json.loads((UI_SOURCE / "app.config").read_text(encoding="utf-8"))
        notifier = (HERE / "package/libexec/sdsync-common").read_bytes()
        app_id = "SYNO.SDS.App.SynologyDriveSync.Instance"
        application = config[app_id]
        self.assertEqual(set(config), {app_id})
        self.assertEqual(application["type"], "app")
        self.assertEqual(application["title"], "Synology Drive Sync")
        self.assertEqual(application["appWindow"], app_id)
        self.assertNotIn("url", application)
        self.assertNotIn(".url", config)
        self.assertIs(application["allUsers"], False)
        self.assertIs(application["allowMultiInstance"], False)
        self.assertIs(application["hidden"], False)
        installed_config = json.loads(build_spk.native_ui_payloads()[0][0])
        expected_installed_application = dict(application)
        expected_installed_application["depend"] = []
        self.assertEqual(
            installed_config,
            {"SynologyDriveSync.js": {app_id: expected_installed_application}},
        )
        self.assertEqual(
            validate_spk.validate_ui_config(json.dumps(installed_config).encode()),
            "ui/SynologyDriveSync.js",
        )
        self.assertFalse((HERE / "conf/resource").exists())
        self.assertFalse((UI / "texts/enu/mails").exists())
        validate_spk.validate_notifier(notifier)
        self.assertNotIn(b"/usr/syno/bin/synonotify", notifier)
        expected = {
            f"notifications:{event}_{suffix}"
            for event in ("sync_succeeded", "sync_failed", "doctor_failed")
            for suffix in ("title", "message")
        }
        self.assertEqual(set(application["preloadTexts"]), expected)

    def test_about_metadata_dependencies_updates_and_safe_links_match_sources(self) -> None:
        cargo = tomllib.loads((HERE.parents[1] / "Cargo.toml").read_text(encoding="utf-8"))
        cargo_lock = tomllib.loads(
            (HERE.parents[1] / "Cargo.lock").read_text(encoding="utf-8")
        )
        ui_package = json.loads((UI_SOURCE / "package.json").read_text(encoding="utf-8"))
        info = validate_spk.parse_info((HERE / "INFO.template").read_bytes())
        app = (UI_SOURCE / "src/App.vue").read_text(encoding="utf-8")
        about_help = (UI / "help/enu/about.html").read_text(encoding="utf-8")
        package = cargo["package"]
        expected_metadata = {
            "project": package["name"],
            "author": "Mariana",
            "authorUrl": "https://github.com/supermarsx",
            "maintainer": info["maintainer"],
            "maintainerUrl": info["maintainer_url"],
            "repository": package["repository"],
            "license": package["license"],
            "coreVersion": package["version"],
            "uiVersion": ui_package["version"],
        }
        for field, value in expected_metadata.items():
            self.assertIn(f'{field}: "{value}"', app)
        self.assertIn("apiSchema: SNAPSHOT_SCHEMA", app)
        self.assertIn(
            "this.snapshot && this.snapshot.package && this.snapshot.package.version",
            app,
        )
        self.assertIn(">Mariana</a>", about_help)

        locked_versions: dict[str, list[str]] = {}
        for resolved_package in cargo_lock["package"]:
            locked_versions.setdefault(resolved_package["name"], []).append(
                resolved_package["version"]
            )

        def locked_version(name: str) -> str:
            versions = locked_versions.get(name, [])
            self.assertEqual(
                len(versions),
                1,
                f"direct Rust dependency {name} must have one Cargo.lock resolution",
            )
            return versions[0]

        rust_dependencies = {
            name: (
                locked_version(name),
                "All platforms",
                f"https://crates.io/crates/{name}",
            )
            for name in cargo["dependencies"]
        }
        target_scopes = {
            'cfg(target_os = "windows")': "Windows",
            'cfg(target_os = "macos")': "macOS",
            'cfg(target_os = "linux")': "Linux",
        }
        self.assertEqual(set(cargo["target"]), set(target_scopes))
        for target, target_values in cargo["target"].items():
            for name in target_values["dependencies"]:
                self.assertNotIn(name, rust_dependencies)
                rust_dependencies[name] = (
                    locked_version(name),
                    target_scopes[target],
                    f"https://crates.io/crates/{name}",
                )

        ui_dependencies = {
            name: (
                version,
                "devDependency",
                f"https://www.npmjs.com/package/{name}",
            )
            for name, version in ui_package["devDependencies"].items()
        }
        package_manager = ui_package["packageManager"]
        package_manager_name = package_manager.split("@", 1)[0]
        ui_dependencies[package_manager_name] = (
            package_manager,
            "packageManager",
            "https://pnpm.io/",
        )

        def app_catalog(constant: str) -> dict[str, tuple[str, str, str]]:
            block = re.search(
                rf"const {constant} = Object\.freeze\(\[([\s\S]*?)\]\);",
                app,
            )
            self.assertIsNotNone(block)
            entries = re.findall(
                r'\{ name: "([^"]+)", pin: "([^"]+)", scope: "([^"]+)", url: "([^"]+)" \}',
                block.group(1),
            )
            self.assertEqual(len(entries), len({name for name, *_ in entries}))
            return {name: (pin, scope, url) for name, pin, scope, url in entries}

        self.assertEqual(app_catalog("ABOUT_RUST_DEPENDENCIES"), rust_dependencies)
        self.assertEqual(app_catalog("ABOUT_UI_DEPENDENCIES"), ui_dependencies)
        self.assertIn(
            "Exact direct versions resolved by the frozen <code>Cargo.lock</code>",
            app,
        )

        help_entries = re.findall(
            r'<li data-package="([^"]+)" data-version="([^"]+)" '
            r'data-scope="([^"]+)"><a href="([^"]+)"[^>]*>([^<]+)</a> '
            r'— ([^<]+) — <code>([^<]+)</code></li>',
            about_help,
        )
        self.assertEqual(len(help_entries), len({name for name, *_ in help_entries}))
        for (
            name,
            version,
            scope,
            _url,
            visible_name,
            visible_scope,
            visible_version,
        ) in help_entries:
            self.assertEqual(
                (visible_name, visible_scope, visible_version),
                (name, scope, version),
            )
        self.assertEqual(
            {
                name: (version, scope, url)
                for name, version, scope, url, *_visible in help_entries
            },
            rust_dependencies | ui_dependencies,
        )
        self.assertIn(
            "exact version resolved by the frozen <code>Cargo.lock</code>",
            about_help,
        )
        for document in (app, about_help):
            self.assertIn(
                "complete transitive Rust release-dependency license inventory",
                document,
            )
            self.assertIn("DSM_UI_THIRD_PARTY_LICENSES.txt", document)
            self.assertIn("Vue is supplied by DSM and is not bundled", document)
            self.assertIn(
                "other pnpm packages whose code is not named in that notice "
                "are used only during the build",
                document,
            )
        self.assertNotIn(
            "complete transitive license inventory ships as",
            about_help,
        )

        for marker in (
            "https://github.com/supermarsx/synology-drive-sync/releases",
            "https://supermarsx.github.io/synology-drive-sync/release-selector.html",
            "does not fetch or install updates",
            "does not configure Package Source discovery",
        ):
            self.assertIn(marker, app)
            self.assertIn(marker, about_help)
        self.assertIn("Package Center <strong>Manual Install</strong>", app)
        self.assertIn("Package Center &gt; Manual Install", about_help)
        validate_spk.validate_external_links(app, "App.vue")
        validate_spk.validate_external_links(about_help, "about.html")

    def test_native_appwindow_is_dark_first_accessible_responsive_and_isolated(self) -> None:
        main = (UI_SOURCE / "src/main.js").read_bytes()
        app = (UI_SOURCE / "src/App.vue").read_bytes()
        api = (UI_SOURCE / "src/api.js").read_bytes()
        css = (UI_SOURCE / "src/styles/native.css").read_bytes()
        validate_spk.validate_native_build_contract(
            main,
            app,
            api,
            css,
            (UI_SOURCE / "webpack.config.js").read_bytes(),
            (UI_SOURCE / "config.define").read_bytes(),
            (UI_SOURCE / "package.json").read_bytes(),
            (UI_SOURCE / "src/ActionIcon.js").read_bytes(),
            (UI_SOURCE / "src/SecurityPanel.vue").read_bytes(),
        )
        app_text = app.decode()
        css_text = css.decode()
        self.assertLess(
            app_text.index('<v-app-instance class-name="SYNO.SDS.App.SynologyDriveSync.Instance">'),
            app_text.index("<v-app-window"),
        )
        self.assertLess(app_text.index("<v-app-window"), app_text.index("</v-app-window>"))
        self.assertLess(
            app_text.index("</main>"),
            app_text.index('<div class="sdsync-toasts"'),
        )
        self.assertLess(
            app_text.index('<div class="sdsync-toasts"'),
            app_text.index('<div v-if="confirmation.visible" class="sdsync-modal-backdrop"'),
        )
        self.assertLess(
            app_text.index('<div v-if="confirmation.visible" class="sdsync-modal-backdrop"'),
            app_text.index("</v-app-window>"),
        )
        for route in (
            "Overview", "Profiles", "Routines", "Health / Doctor",
            "Activity / Logs", "Notifications", "Security", "Settings", "About",
        ):
            self.assertIn(f'title: "{route}"', app_text)
        for marker in (
            'aria-live="polite"', 'id="sdsync-page-title"',
            'aria-labelledby="sdsync-page-title"',
            "<v-button", "<v-form", "<v-form-item", "<v-single-select",
            "beforeDestroy()", "this.stopTimers();",
            "this.disposed = true;", "this.abortController.abort();",
            "this.toastTimers.forEach((timer) => window.clearTimeout(timer))",
            'document.removeEventListener("visibilitychange", this.visibilityHandler)',
            'this.mediaQuery.removeEventListener("change", this.mediaHandler)',
            ':aria-label="item.title"', ':title="item.title"',
            'type="time" aria-label="Window starts"',
            'type="time" aria-label="Window ends"',
            'multiple size="4" aria-label="Wait for routines"',
            'role="dialog" aria-modal="true"',
            'document.addEventListener("keydown", this.confirmationKeyHandler, true)',
            'document.removeEventListener("keydown", this.confirmationKeyHandler, true)',
            'if (this.route === "profiles" && route !== "profiles") this.closeProfile();',
            'const awaitTerminal = kind === "doctor";',
            'title="Synology Drive Sync"',
            'this.snapshot && this.snapshot.package && this.snapshot.package.version',
            'https://github.com/supermarsx/synology-drive-sync/releases',
            'https://supermarsx.github.io/synology-drive-sync/release-selector.html',
            'target="_blank" rel="noopener noreferrer"',
        ):
            self.assertIn(marker, app_text)
        for forbidden in (
            "<iframe", "index.html", "document.documentElement",
            "window.location.hash", "hashchange", "v-html", ".innerHTML", "eval(",
            ':title="windowTitle"', "Your sync estate, at a glance.", "sdsync-hero",
            "sdsync-check-grid", "sdsync-editor-placeholder", "sdsync-section-heading",
        ):
            self.assertNotIn(forbidden, app_text)
        self.assertTrue(css_text.startswith(".sdsync-app {"))
        self.assertIn(".sdsync-app.is-light", css_text)
        self.assertGreaterEqual(
            len(re.findall(r"@media \(max-width: [1-9][0-9]*px\)", css_text)),
            2,
        )
        self.assertIn("@media (prefers-reduced-motion: reduce)", css_text)
        self.assertIn(":focus-visible", css_text)
        self.assertNotRegex(css_text, r"(^|[},])\s*(?::root|html\b|body\b)")

    def test_native_profile_routine_health_notification_and_settings_surfaces_are_complete(self) -> None:
        app = (UI_SOURCE / "src/App.vue").read_text(encoding="utf-8")
        api = (UI_SOURCE / "src/api.js").read_text(encoding="utf-8")
        manager = (HERE / "package/bin/sdsync-dsm").read_text(encoding="utf-8")
        for field in (
            "allow_empty_source", "ca_certificate", "connect_timeout",
            "danger_invalid_certs", "excludes", "max_rate", "remote_log_mode",
            "remote_log_token", "retry_backoff_seconds", "retry_count",
            "time_window_end", "time_window_start", "weekdays",
        ):
            self.assertIn(field, app)
        for marker in (
            "keep", "replace", "clear", "interval", "daily", "realtime",
            "Reachable", "Writable", "Latency", "Last success", "Free space",
            "failure_threshold", "cooldown_seconds", "Save interface settings",
            "sdsync-settings-actions",
        ):
            self.assertIn(marker, app)
        for operation in (
            "configure-profile", "remove-profile", "set-default", "set-secret",
            "schedule", "routine", "remove-routine", "alert-policy", "action",
            "security-policy", "client-event",
        ):
            self.assertIn(f'"{operation}"', api)
        self.assertNotRegex(app, r"localStorage[^\n]*(?:password|totp|remote_log_token)")
        self.assertIn("set-remote-log-token NAME [--from-file FILE]", manager)
        self.assertIn("remove-password NAME | remove-totp NAME | remove-remote-log-token NAME", manager)

    def test_native_api_source_is_canonical_and_appwindow_safe(self) -> None:
        api = (UI_SOURCE / "src/api.js").read_text(encoding="utf-8")
        app = (UI_SOURCE / "src/App.vue").read_text(encoding="utf-8")
        validate_spk.validate_native_api_source(api.encode())
        self.assertEqual(
            re.findall(r'["\']([^"\']*api\.cgi)["\']', api),
            ["/webman/3rdparty/synology-drive-sync/api.cgi"],
        )
        self.assertEqual(api.count('credentials: "same-origin"'), 2)
        self.assertEqual(api.count('"X-SDSYNC-Request"'), 1)
        self.assertIn("function authenticatedHeaders(headers)", api)
        self.assertIn('auth: { signal: undefined }', app)
        for source in (api, app):
            for forbidden in (
                "login.cgi", "consumeLaunchToken", "window.location", "window.history",
                "history.replaceState", "X-SYNO-TOKEN", "SynoToken", "synotoken",
                "launch token", "hashchange",
            ):
                self.assertNotIn(forbidden, source)

    def test_native_api_authentication_and_headers_behave_in_appwindow_context(self) -> None:
        node = shutil.which("node")
        if not node:
            self.skipTest("node is not available")

        api_source = (UI_SOURCE / "src/api.js").read_text(encoding="utf-8")
        executable_source = re.sub(r"^export\s+", "", api_source, flags=re.MULTILINE)
        harness = "\n".join(
            (
                '"use strict";',
                'const assert = require("node:assert/strict");',
                executable_source,
                r'''
const requests = [];
global.window = {
  crypto: {
    getRandomValues: function (values) { values.fill(1); return values; }
  },
  setTimeout: setTimeout,
  clearTimeout: clearTimeout
};
global.fetch = async function (url, options) {
  requests.push({ url: url, options: options });
  let payload = { ok: true };
  if (options.method === "POST") {
    const request = JSON.parse(options.body);
    payload = {
      schema: QUEUED_SCHEMA,
      state: "queued",
      request_id: request.request_id,
      job_id: "0".repeat(48)
    };
  }
  return {
    redirected: false,
    ok: true,
    status: 200,
    headers: { get: function () { return "application/json; charset=utf-8"; } },
    text: async function () { return JSON.stringify(payload); }
  };
};

(async function () {
  const cookieOnly = {};
  const signalMarker = { aborted: false };
  const adversarialFields = { token: "must-be-ignored", invalid: true, signal: signalMarker };

  await apiGet(cookieOnly, "snapshot");
  const getCookieOnly = requests[requests.length - 1];
  await apiGet(adversarialFields, "snapshot");
  const getAdversarial = requests[requests.length - 1];
  await apiPost(cookieOnly, "csrf-token", "set-default", { name: "profile" }, false);
  const postCookieOnly = requests[requests.length - 1];
  await apiPost(adversarialFields, "csrf-token", "set-default", { name: "profile" }, false);
  const postAdversarial = requests[requests.length - 1];

  assert.equal(
    getCookieOnly.url,
    "/webman/3rdparty/synology-drive-sync/api.cgi?action=snapshot"
  );
  assert.equal(postCookieOnly.url, "/webman/3rdparty/synology-drive-sync/api.cgi");
  for (const request of [getCookieOnly, getAdversarial]) {
    assert.equal(request.options.method, "GET");
    assert.equal(request.options.credentials, "same-origin");
    assert.deepEqual(request.options.headers, {
      Accept: "application/json",
      "X-SDSYNC-Request": "1"
    });
  }
  assert.equal(getAdversarial.options.signal, signalMarker);

  for (const request of [postCookieOnly, postAdversarial]) {
    assert.equal(request.options.method, "POST");
    assert.equal(request.options.credentials, "same-origin");
    assert.deepEqual(request.options.headers, {
      "Content-Type": "application/json",
      Accept: "application/json",
      "X-SDSYNC-CSRF": "csrf-token",
      "X-SDSYNC-Request": "1"
    });
  }
  assert.equal(postAdversarial.options.signal, signalMarker);

  const cancellation = new AbortController();
  const pendingDelay = delay(60000, cancellation.signal);
  cancellation.abort();
  await assert.rejects(pendingDelay, /DSM UI request was cancelled/);
})().catch(function (error) {
  process.stderr.write(String(error && error.stack ? error.stack : error));
  process.exitCode = 1;
});
''',
            )
        )
        completed = subprocess.run(
            [node, "-"],
            input=harness,
            capture_output=True,
            text=True,
            timeout=20,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_native_queued_result_semantics_are_behavioral(self) -> None:
        node = shutil.which("node")
        if not node:
            self.skipTest("node is not available")

        api_source = (UI_SOURCE / "src/api.js").read_text(encoding="utf-8")
        executable_source = re.sub(r"^export\s+", "", api_source, flags=re.MULTILINE)
        harness = "\n".join(
            (
                '"use strict";',
                'const assert = require("node:assert/strict");',
                executable_source,
                r'''
const jobId = "a".repeat(48);
const pending = () => ({
  schema: RESULT_STATUS_SCHEMA,
  job_id: jobId,
  state: "pending"
});
const complete = (result) => ({
  schema: RESULT_STATUS_SCHEMA,
  job_id: jobId,
  state: "complete",
  result: result
});
const result = (ok, message, output) => ({
  schema: RESULT_SCHEMA,
  ok: ok,
  message: message,
  output: output
});
const jsonResponse = (payload, status = 200) => ({
  redirected: false,
  ok: status >= 200 && status < 300,
  status: status,
  headers: { get: () => "application/json; charset=utf-8" },
  text: async () => JSON.stringify(payload)
});

const scenarios = [];
let observations = [];
let observationCount = 0;
global.window = {
  location: {
    origin: "https://nas.example",
    href: "https://nas.example/webman/index.cgi"
  },
  history: { replaceState: function () {} },
  crypto: {
    getRandomValues: function (values) { values.fill(2); return values; }
  },
  setTimeout: setTimeout,
  clearTimeout: clearTimeout
};
global.fetch = async function (_url, options) {
  if (options.method === "POST") {
    observations = scenarios.shift().slice();
    const request = JSON.parse(options.body);
    return jsonResponse({
      schema: QUEUED_SCHEMA,
      state: "queued",
      request_id: request.request_id,
      job_id: jobId
    });
  }
  observationCount += 1;
  const next = observations.shift();
  if (next instanceof Error) throw next;
  if (!next) throw new Error("test exhausted queued-result observations");
  return jsonResponse(next.payload || next, next.status || 200);
};

async function capture(promise) {
  try {
    await promise;
  } catch (error) {
    return error;
  }
  throw new Error("expected promise to reject");
}

(async function () {
  const auth = { token: "", invalid: false };
  const action = "set-default";
  const payload = { name: "profile" };

  scenarios.push([
    ...Array.from({ length: 70 }, pending),
    complete(result(true, "done", "terminal output"))
  ]);
  observationCount = 0;
  const beyondOldHorizon = await apiPost(
    auth, "csrf", action, payload, true, 0
  );
  assert.equal(beyondOldHorizon.ok, true);
  assert.equal(beyondOldHorizon.output, "terminal output");
  assert.equal(observationCount, 71);

  scenarios.push([
    new Error("temporary transport loss"),
    new Error("temporary authentication observation loss"),
    pending(),
    complete(result(true, "observed", "recovered output"))
  ]);
  const recovered = await apiPost(auth, "csrf", action, payload, true, 0);
  assert.equal(recovered.output, "recovered output");

  scenarios.push(Array.from(
    { length: 5 },
    () => new Error("result observation unavailable")
  ));
  const unobservable = await capture(apiPost(auth, "csrf", action, payload, true, 0));
  assert.equal(unobservable instanceof QueuedOutcomeUnknownError, true);
  assert.equal(unobservable.outcomeUnknown, true);
  assert.equal(unobservable.jobId, jobId);
  assert.match(unobservable.message, /accepted the operation/i);
  assert.match(unobservable.message, /do not retry/i);

  scenarios.push([{ schema: "invalid", job_id: jobId, state: "pending" }]);
  const malformed = await capture(apiPost(auth, "csrf", action, payload, true, 0));
  assert.equal(malformed instanceof QueuedOutcomeUnknownError, true);
  assert.equal(malformed.outcomeUnknown, true);
  assert.equal(malformed.jobId, jobId);

  scenarios.push([{
    schema: RESULT_STATUS_SCHEMA,
    job_id: jobId,
    state: "expired_or_missing",
    result: { message: "retention elapsed" }
  }]);
  const expired = await capture(apiPost(auth, "csrf", action, payload, true, 0));
  assert.equal(expired instanceof QueuedOutcomeUnknownError, true);
  assert.match(expired.message, /retention elapsed/);

  scenarios.push([complete(result(false, "doctor failed", "doctor raw output"))]);
  const terminalFailure = await capture(
    apiPost(auth, "csrf", action, payload, true, 0)
  );
  assert.equal(terminalFailure instanceof QueuedOutcomeUnknownError, false);
  assert.equal(terminalFailure.outcomeUnknown, undefined);
  assert.equal(terminalFailure.message, "doctor failed");
  assert.equal(terminalFailure.resultOutput, "doctor raw output");
})().catch(function (error) {
  process.stderr.write(String(error && error.stack ? error.stack : error));
  process.exitCode = 1;
});
''',
            )
        )
        completed = subprocess.run(
            [node, "-"],
            input=harness,
            capture_output=True,
            text=True,
            timeout=20,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_native_appwindow_interactions_and_operation_modes_are_behavioral(self) -> None:
        node = shutil.which("node")
        if not node:
            self.skipTest("node is not available")

        app = (UI_SOURCE / "src/App.vue").read_text(encoding="utf-8")
        script_match = re.search(r"<script>\s*(.*?)\s*</script>", app, re.DOTALL)
        self.assertIsNotNone(script_match)
        executable = re.sub(
            r'import\s*\{.*?\}\s*from\s*"\./api";\s*',
            "",
            script_match.group(1),  # type: ignore[union-attr]
            count=1,
            flags=re.DOTALL,
        )
        executable = re.sub(
            r'import\s+SecurityPanel\s+from\s+"\./SecurityPanel\.vue";\s*',
            "",
            executable,
            count=1,
        )
        executable = executable.replace("export default {", "const AppComponent = {", 1)
        executable = executable.replace("apiPost(", "apiPostSpy(")
        harness = "\n".join(
            (
                '"use strict";',
                'const assert = require("node:assert/strict");',
                r'''
const ACTIONS = { execute: "action" };
const SNAPSHOT_SCHEMA = "sdsync.dsm-api.v1";
const ActionIcon = {};
const SecurityPanel = {};
const boundedText = (value, fallback = "") =>
  String(typeof value === "string" && value ? value : fallback).slice(0, 65536);
const formatBytes = (value) => String(value);
const formatDate = (value) => String(value);
const formatDuration = (value) => String(value);
const postCalls = [];
let postFailure = null;
async function apiPostSpy(_auth, _csrf, action, payload, awaitTerminal) {
  postCalls.push({ action, payload, awaitTerminal });
  if (postFailure) throw postFailure;
  return awaitTerminal
    ? { output: "doctor terminal output" }
    : { state: "queued", job_id: "b".repeat(48) };
}
global.window = { clearTimeout: function () {} };
''',
                executable,
                r'''
const methods = AppComponent.methods;

function operationContext() {
  const context = {
    canMutate: true,
    canRunOperations: true,
    canAllowDestructive: true,
    canRunDoctorWrite: true,
    capabilities: { write_test: true },
    operationBusy: false,
    disposed: false,
    auth: {},
    csrfToken: "csrf",
    diagnostic: {},
    toasts: [],
    toast: function (title, message, error) {
      this.toasts.push({ title, message, error });
    },
    refreshSnapshot: async function () {},
    reportMutationError: function (...args) {
      return methods.reportMutationError.apply(this, args);
    },
    hasCapability: function (name) { return this.capabilities[name] === true; }
  };
  return context;
}

function bind(context, names) {
  names.forEach((name) => {
    context[name] = (...args) => methods[name].apply(context, args);
  });
}

(async function () {
  const operation = operationContext();
  await methods.executeOperation.call(operation, "doctor", {
    scope: "all", write_test: false, allow_delete: null, max_total_delete: null
  });
  await methods.executeOperation.call(operation, "plan", {
    scope: "all", write_test: null, allow_delete: false, max_total_delete: 0
  });
  await methods.executeOperation.call(operation, "run", {
    scope: "all", write_test: null, allow_delete: false, max_total_delete: 0
  });
  assert.deepEqual(postCalls.map((call) => call.awaitTerminal), [true, false, false]);
  assert.deepEqual(
    operation.toasts.map((toast) => toast.title),
    ["Doctor completed", "Plan queued", "Run queued"]
  );
  assert.deepEqual(operation.diagnostic, {
    title: "Doctor completed",
    output: "doctor terminal output"
  });

  postFailure = new Error("doctor failed");
  postFailure.resultOutput = "doctor preserved raw output";
  await methods.executeOperation.call(operation, "doctor", {
    scope: "all", write_test: false, allow_delete: null, max_total_delete: null
  });
  assert.deepEqual(operation.diagnostic, {
    title: "Doctor failed",
    output: "doctor preserved raw output"
  });
  postFailure = null;

  const beforeBusy = postCalls.length;
  operation.operationBusy = true;
  await methods.executeOperation.call(operation, "doctor", {
    scope: "all", write_test: false, allow_delete: null, max_total_delete: null
  });
  assert.equal(postCalls.length, beforeBusy);

  const routeContext = {
    routes: [{ id: "profiles" }, { id: "overview" }],
    route: "profiles",
    logTimer: 0,
    profileEditorOpen: true,
    selectedProfile: "private",
    secretModes: {
      password: "replace", totp: "replace", remote_log_token: "replace"
    },
    secretValues: {
      password: "password", totp: "totp", remote_log_token: "token"
    },
    refreshLogs: function () {}
  };
  bind(routeContext, ["clearSecrets", "closeProfile"]);
  methods.navigate.call(routeContext, "overview");
  assert.deepEqual(routeContext.secretValues, {
    password: "", totp: "", remote_log_token: ""
  });
  assert.deepEqual(routeContext.secretModes, {
    password: "keep", totp: "keep", remote_log_token: "keep"
  });
  assert.equal(routeContext.profileEditorOpen, false);
  assert.equal(routeContext.selectedProfile, "");

  const focusLog = [];
  const listeners = [];
  function focusable(name) {
    return {
      name,
      disabled: false,
      isConnected: true,
      matches: () => true,
      getAttribute: () => null,
      focus: function () {
        global.document.activeElement = this;
        focusLog.push(name);
      }
    };
  }
  const prior = focusable("prior");
  const cancel = focusable("cancel");
  const accept = focusable("accept");
  const dialog = {
    matches: () => true,
    focus: () => focusLog.push("dialog"),
    contains: (element) => element === cancel || element === accept,
    querySelectorAll: () => [cancel, accept]
  };
  global.document = {
    activeElement: prior,
    addEventListener: function (type, handler, capture) {
      listeners.push({ action: "add", type, handler, capture });
    },
    removeEventListener: function (type, handler, capture) {
      listeners.push({ action: "remove", type, handler, capture });
    }
  };
  const dialogContext = {
    confirmation: {
      visible: false, title: "", message: "", button: "Confirm", resolve: null
    },
    confirmationPriorFocus: null,
    confirmationKeyHandler: null,
    disposed: false,
    $refs: {
      confirmationDialog: dialog,
      confirmationCancel: cancel,
      confirmationAccept: accept
    },
    $nextTick: (callback) => callback()
  };
  bind(dialogContext, [
    "confirmationElement",
    "confirmationFocusables",
    "handleConfirmationKeydown",
    "removeConfirmationKeyHandler",
    "settleConfirmation"
  ]);
  const confirmation = methods.confirmAction.call(
    dialogContext, "Danger", "Confirm the action", "Continue"
  );
  assert.equal(dialogContext.confirmation.visible, true);
  assert.equal(focusLog[focusLog.length - 1], "cancel");
  assert.deepEqual(
    listeners.map((entry) => [entry.action, entry.type, entry.capture]),
    [["add", "keydown", true]]
  );

  global.document.activeElement = accept;
  const forwardTab = {
    key: "Tab", shiftKey: false, prevented: false,
    preventDefault: function () { this.prevented = true; },
    stopPropagation: function () {}
  };
  methods.handleConfirmationKeydown.call(dialogContext, forwardTab);
  assert.equal(forwardTab.prevented, true);
  assert.equal(global.document.activeElement, cancel);

  global.document.activeElement = cancel;
  const reverseTab = {
    key: "Tab", shiftKey: true, prevented: false,
    preventDefault: function () { this.prevented = true; },
    stopPropagation: function () {}
  };
  methods.handleConfirmationKeydown.call(dialogContext, reverseTab);
  assert.equal(reverseTab.prevented, true);
  assert.equal(global.document.activeElement, accept);

  const escape = {
    key: "Escape", prevented: false, stopped: false,
    preventDefault: function () { this.prevented = true; },
    stopPropagation: function () { this.stopped = true; }
  };
  methods.handleConfirmationKeydown.call(dialogContext, escape);
  assert.equal(await confirmation, false);
  assert.equal(escape.prevented, true);
  assert.equal(escape.stopped, true);
  assert.equal(dialogContext.confirmation.visible, false);
  assert.equal(global.document.activeElement, prior);
  assert.deepEqual(
    listeners.map((entry) => [entry.action, entry.type, entry.capture]),
    [["add", "keydown", true], ["remove", "keydown", true]]
  );
})().catch(function (error) {
  process.stderr.write(String(error && error.stack ? error.stack : error));
  process.exitCode = 1;
});
''',
            )
        )
        completed = subprocess.run(
            [node, "-"],
            input=harness,
            capture_output=True,
            text=True,
            timeout=20,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_native_run_details_and_zero_routine_values_are_preserved(self) -> None:
        app = (UI_SOURCE / "src/App.vue").read_text(encoding="utf-8")
        api = (UI_SOURCE / "src/api.js").read_text(encoding="utf-8")

        details = re.search(
            r'<dl class="sdsync-definition-grid">(.*?)</dl>', app, re.DOTALL
        )
        self.assertIsNotNone(details)
        self.assertEqual(
            re.findall(r"<dt>([^<]+)</dt>", details.group(1)),
            ["Operation", "State", "Scope", "Started", "Finished"],
        )
        for marker in (
            "runOperation() { return boundedText(this.run.operation, \"Unavailable\"); }",
            "runScope() { return boundedText(this.run.scope, \"Unavailable\"); }",
            "numberOr(routine.retry_count, 2)",
            "numberOr(routine.max_total_delete, 100)",
        ):
            self.assertIn(marker, app)
        self.assertIn("return Number.isFinite(parsed) ? parsed : fallback;", api)

    def test_javascript_parses_when_node_is_available(self) -> None:
        node = shutil.which("node")
        if not node:
            self.skipTest("node is not installed")
        for source in (
            UI_SOURCE / "src/main.js",
            UI_SOURCE / "src/api.js",
            UI_SOURCE / "src/ActionIcon.js",
            UI_SOURCE / "dist/SynologyDriveSync.js",
        ):
            result = subprocess.run(
                [node, "--check", str(source)],
                capture_output=True,
                text=True,
                timeout=20,
            )
            self.assertEqual(result.returncode, 0, f"{source.name}: {result.stderr}")

    def test_icon_family_is_deterministic_snapshotted_and_inside_safe_bounds(self) -> None:
        expected_hashes = {
            16: "23fc20ce25acd0a907508134369affe48babdb8eeeb72b6d4353671937a67128",
            24: "204efbcf279f78ad6b4af1891f8b3522831cd47d7dfa50bb84d6a0c8729e4027",
            32: "a4d9ff5108c02e43cd3a086f280f942d0dcdebb645a884a13fef922d6cea0a7f",
            48: "b3431fe0f4a0c54a126dbf7691b0822f0fdf171bbecc83dbde5373c85ab7d9b4",
            64: "bf5b241038e696f7f7c56c1eeecb233beb59353c0310f026e6ae79c83e9b180d",
            72: "6468ba3c0b56d68b577dd9b17a284880ba8110f21b80f19949bae1b1343f0179",
            256: "6c431626134d444c149d0a83fd5add537693bee81d9f4a3ff2269c8015e111f2",
        }
        self.assertEqual(tuple(expected_hashes), build_spk.UI_ICON_SIZES)
        for size, expected_hash in expected_hashes.items():
            payload = build_spk.png_icon(size)
            self.assertEqual(hashlib.sha256(payload).hexdigest(), expected_hash)
            self.assertEqual(validate_spk.png_dimensions(payload), (size, size))
            minimum_x, maximum_x, minimum_y, maximum_y = self._alpha_bounds(payload, size)
            self.assertGreaterEqual(minimum_x, 1)
            self.assertGreaterEqual(minimum_y, 1)
            self.assertLessEqual(maximum_x, size - 2)
            self.assertLessEqual(maximum_y, size - 2)
        validate_spk.validate_svg_icon((UI / "images/icon.svg").read_bytes())

    def test_icon_arrow_apices_stay_ahead_of_bodies_at_every_size(self) -> None:
        arrows = (
            ("top", build_spk.ICON_TOP_BODY, build_spk.ICON_TOP_TIP),
            ("bottom", build_spk.ICON_BOTTOM_BODY, build_spk.ICON_BOTTOM_TIP),
        )
        for name, body, tip in arrows:
            triangle = build_spk._arrow_triangle(body, tip)
            apex = triangle[0]
            base = body[-1]
            rear = body[-2]
            direction = (tip[0] - base[0], tip[1] - base[1])
            prior = (base[0] - rear[0], base[1] - rear[1])
            with self.subTest(arrow=name, contract="apex"):
                self.assertGreater(direction[0] * prior[0] + direction[1] * prior[1], 0)
                self.assertAlmostEqual((triangle[1][0] + triangle[2][0]) / 2, base[0])
                self.assertAlmostEqual((triangle[1][1] + triangle[2][1]) / 2, base[1])
                self.assertTrue(build_spk._triangle_contains(*apex, *triangle))
                self.assertFalse(
                    build_spk._trace_contains(
                        *apex,
                        body,
                        build_spk.ICON_ARROW_HALF_THICKNESS,
                        stop_at_final_base=True,
                    )
                )

        supersample = 4
        for size in build_spk.UI_ICON_SIZES:
            resolution = size * supersample
            for name, body, tip in arrows:
                triangle = build_spk._arrow_triangle(body, tip)
                base = body[-1]
                direction = (tip[0] - base[0], tip[1] - base[1])
                head_samples = 0
                overlap_samples = 0
                forward_head_pixels: set[tuple[int, int]] = set()
                for sample_y in range(resolution):
                    normalized_y = (sample_y + 0.5) / resolution
                    for sample_x in range(resolution):
                        normalized_x = (sample_x + 0.5) / resolution
                        on_head = build_spk._triangle_contains(
                            normalized_x, normalized_y, *triangle
                        )
                        if not on_head:
                            continue
                        head_samples += 1
                        on_body = build_spk._trace_contains(
                            normalized_x,
                            normalized_y,
                            body,
                            build_spk.ICON_ARROW_HALF_THICKNESS,
                            stop_at_final_base=True,
                        )
                        if on_body:
                            overlap_samples += 1
                            continue
                        forward = (
                            (normalized_x - base[0]) * direction[0]
                            + (normalized_y - base[1]) * direction[1]
                        )
                        if forward > 0:
                            forward_head_pixels.add(
                                (sample_x // supersample, sample_y // supersample)
                            )
                with self.subTest(size=size, arrow=name, contract="base-overlap"):
                    overlap_ratio = overlap_samples / head_samples
                    self.assertLess(overlap_ratio, 0.02)
                    self.assertTrue(forward_head_pixels)

    def test_svg_icon_matches_raster_arrow_geometry_and_palette(self) -> None:
        svg = (UI / "images/icon.svg").read_text(encoding="utf-8")

        def scalar(value: float) -> str:
            return f"{value:.3f}".rstrip("0").rstrip(".")

        stroke_width = scalar(512 * build_spk.ICON_ARROW_HALF_THICKNESS)
        for body, color in (
            (build_spk.ICON_TOP_BODY, build_spk.ICON_TOP),
            (build_spk.ICON_BOTTOM_BODY, build_spk.ICON_BOTTOM),
        ):
            points = " ".join(
                f"{x * 256:.3f},{y * 256:.3f}" for x, y in body
            )
            color_hex = "#" + "".join(f"{round(channel):02x}" for channel in color)
            marker = f'<polyline points="{points}"'
            self.assertIn(marker, svg)
            self.assertRegex(
                svg,
                re.escape(marker) + rf'[^>]+stroke="{color_hex}"[^>]+stroke-width="{stroke_width}"',
            )

        for body, tip, color in (
            (build_spk.ICON_TOP_BODY, build_spk.ICON_TOP_TIP, build_spk.ICON_TOP),
            (build_spk.ICON_BOTTOM_BODY, build_spk.ICON_BOTTOM_TIP, build_spk.ICON_BOTTOM),
        ):
            points = " ".join(
                f"{x * 256:.3f},{y * 256:.3f}"
                for x, y in build_spk._arrow_triangle(body, tip)
            )
            color_hex = "#" + "".join(f"{round(channel):02x}" for channel in color)
            self.assertIn(f'<polygon points="{points}" fill="{color_hex}"/>', svg)

        palette = (
            build_spk.ICON_BACKGROUND,
            build_spk.ICON_BORDER,
            build_spk.ICON_ORBIT,
            build_spk.ICON_TOP,
            build_spk.ICON_BOTTOM,
            build_spk.ICON_CENTER,
        )
        for color in palette:
            color_hex = "#" + "".join(f"{round(channel):02x}" for channel in color)
            self.assertIn(color_hex, svg)
        orbit = " ".join(
            f"{x * 256:.3f},{y * 256:.3f}" for x, y in build_spk.ICON_ORBIT_POINTS
        )
        self.assertIn(f'<polyline points="{orbit}"', svg)
        self.assertNotRegex(svg, r"#(?:65e6c7|2fb69d|d9fff5|08110f)")
        for body, tip in (
            (build_spk.ICON_TOP_BODY, build_spk.ICON_TOP_TIP),
            (build_spk.ICON_BOTTOM_BODY, build_spk.ICON_BOTTOM_TIP),
        ):
            body_marker = " ".join(f"{x * 256:.3f},{y * 256:.3f}" for x, y in body)
            head_marker = " ".join(
                f"{x * 256:.3f},{y * 256:.3f}"
                for x, y in build_spk._arrow_triangle(body, tip)
            )
            self.assertLess(svg.index(body_marker), svg.index(head_marker))

    def test_payload_contains_offline_ui_icons_and_identical_helper_modes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            core = root / "core"
            helper = root / "helper"
            core.write_bytes(b"core-fixture")
            helper.write_bytes(b"api-fixture")
            payload, installed_size = build_spk.payload_archive(core, helper)
        with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as archive:
            members = {member.name: member for member in archive.getmembers()}
            self.assertEqual(members["bin/sdsync-dsm-api"].mode & 0o7777, 0o755)
            self.assertEqual(members["ui/api.cgi"].mode & 0o7777, 0o755)
            self.assertEqual(archive.extractfile(members["bin/sdsync-dsm-api"]).read(), b"api-fixture")
            self.assertEqual(archive.extractfile(members["ui/api.cgi"]).read(), b"api-fixture")
            self.assertFalse(
                any(member.mode & 0o6000 for member in members.values()),
                "package.tgz must not expose setuid/setgid archive entries",
            )
            for size in build_spk.UI_ICON_SIZES:
                name = f"ui/images/icon_{size}.png"
                self.assertIn(name, members)
                self.assertEqual(validate_spk.png_dimensions(archive.extractfile(members[name]).read()), (size, size))
            for name in (
                "ui/config",
                "ui/index.html",
                "ui/SynologyDriveSync.js",
                "ui/style.css",
                "ui/images/icon.svg",
                "ui/texts/enu/strings",
                "ui/helptoc.conf",
                *(f"ui/help/enu/{page}.html" for page in build_spk.UI_HELP_PAGES),
            ):
                self.assertIn(name, members)
            self.assertEqual(members["ui/index.html"].mode & 0o7777, 0o644)
            direct_launcher = archive.extractfile(members["ui/index.html"]).read()
            self.assertEqual(direct_launcher, (UI / "index.html").read_bytes())
            validate_spk.validate_direct_launcher(direct_launcher)
            for name in ("ui/app.js", "ui/app.css"):
                self.assertNotIn(name, members)
            ui_config = archive.extractfile(members["ui/config"]).read()
            self.assertEqual(
                validate_spk.validate_ui_config(ui_config),
                "ui/SynologyDriveSync.js",
            )
            validate_spk.validate_native_bundle(
                archive.extractfile(members["ui/SynologyDriveSync.js"]).read(),
                archive.extractfile(members["ui/style.css"]).read(),
            )
            validate_spk.validate_dsm_help(
                archive.extractfile(members["ui/helptoc.conf"]).read(),
                {
                    page: archive.extractfile(
                        members[f"ui/help/enu/{page}.html"]
                    ).read()
                    for page in build_spk.UI_HELP_PAGES
                },
            )
            self.assertNotIn("ui/texts/enu/mails", members)
            self.assertEqual(installed_size, sum(member.size for member in members.values() if member.isfile()))

    def test_builder_refuses_setid_archive_metadata(self) -> None:
        with self.assertRaisesRegex(build_spk.PackageError, "must not carry setuid/setgid"):
            build_spk.tar_info("ui/api.cgi", 0o4755, 1)

    def test_static_validators_reject_security_and_registration_tampering(self) -> None:
        source = {
            "main": (UI_SOURCE / "src/main.js").read_bytes(),
            "app": (UI_SOURCE / "src/App.vue").read_bytes(),
            "api": (UI_SOURCE / "src/api.js").read_bytes(),
            "css": (UI_SOURCE / "src/styles/native.css").read_bytes(),
            "webpack": (UI_SOURCE / "webpack.config.js").read_bytes(),
            "config_define": (UI_SOURCE / "config.define").read_bytes(),
            "package": (UI_SOURCE / "package.json").read_bytes(),
            "action_icon": (UI_SOURCE / "src/ActionIcon.js").read_bytes(),
            "security_panel": (UI_SOURCE / "src/SecurityPanel.vue").read_bytes(),
        }

        def validate_build(**overrides: bytes) -> None:
            payloads = dict(source)
            payloads.update(overrides)
            validate_spk.validate_native_build_contract(
                payloads["main"],
                payloads["app"],
                payloads["api"],
                payloads["css"],
                payloads["webpack"],
                payloads["config_define"],
                payloads["package"],
                payloads["action_icon"],
                payloads["security_panel"],
            )

        with self.assertRaisesRegex(
            validate_spk.ValidationError,
            "shared ActionIcon validation requires both component sources",
        ):
            validate_spk.validate_native_build_contract(
                source["main"],
                source["app"],
                source["api"],
                source["css"],
                source["webpack"],
                source["config_define"],
                source["package"],
                source["action_icon"],
            )
        self.assertIn(b"overview: [", source["action_icon"])
        with self.assertRaisesRegex(
            validate_spk.ValidationError,
            "shared ActionIcon source is missing canonical icon 'overview'",
        ):
            validate_build(
                action_icon=source["action_icon"].replace(
                    b"overview: [", b"overview_removed: [", 1
                )
            )
        security_import = b'import { ActionIcon } from "./ActionIcon";'
        self.assertIn(security_import, source["security_panel"])
        with self.assertRaisesRegex(
            validate_spk.ValidationError,
            "native DSM security panel is missing shared ActionIcon contract",
        ):
            validate_build(
                security_panel=source["security_panel"].replace(
                    security_import,
                    b'import { ActionIcon } from "./MissingActionIcon";',
                    1,
                )
            )

        with self.assertRaisesRegex(validate_spk.ValidationError, "AppWindow structure"):
            validate_build(app=source["app"].replace(b"<v-app-window", b"<section", 1))
        for name, marker in (
            ("iframe", b"<iframe"),
            ("legacy launcher", b"index.html"),
            ("global theme", b"document.documentElement"),
            ("hash router", b"window.location.hash"),
        ):
            with self.subTest(name=name), self.assertRaisesRegex(
                validate_spk.ValidationError, "forbidden launcher or DOM construct"
            ):
                validate_build(app=source["app"] + b"\n" + marker)
        with self.assertRaisesRegex(validate_spk.ValidationError, "destruction cleanup"):
            validate_build(
                app=source["app"].replace(b"beforeDestroy()", b"destroyed()", 1)
            )
        with self.assertRaisesRegex(validate_spk.ValidationError, "destruction cleanup"):
            validate_build(
                app=source["app"].replace(b"this.abortController.abort();", b"", 1)
            )
        with self.assertRaisesRegex(validate_spk.ValidationError, "canonical absolute"):
            validate_build(
                api=source["api"].replace(
                    b'"/webman/3rdparty/synology-drive-sync/api.cgi"',
                    b'"./api.cgi"',
                    1,
                )
            )
        with self.assertRaisesRegex(validate_spk.ValidationError, "X-SDSYNC-Request"):
            validate_build(
                api=source["api"].replace(
                    b'"X-SDSYNC-Request": "1"',
                    b'"X-SDSYNC-Request": "0"',
                    1,
                )
            )
        with self.assertRaisesRegex(validate_spk.ValidationError, "GET requests"):
            validate_build(
                api=source["api"].replace(
                    b"headers: authenticatedHeaders({ Accept:",
                    b"headers: ({ Accept:",
                    1,
                )
            )
        post_call = source["api"].rfind(b"headers: authenticatedHeaders(")
        self.assertGreater(post_call, -1)
        with self.assertRaisesRegex(validate_spk.ValidationError, "POST requests"):
            validate_build(
                api=source["api"][:post_call]
                + source["api"][post_call:].replace(
                    b"headers: authenticatedHeaders(", b"headers: (", 1
                )
            )
        with self.assertRaisesRegex(validate_spk.ValidationError, "launch-token authentication"):
            validate_build(
                api=source["api"].replace(
                    b"function authenticatedHeaders(headers)",
                    b"function authenticatedHeaders(auth, headers)",
                    1,
                )
            )
        for name, marker in (
            ("token parser", b"\nfunction consumeLaunchToken() {}\n"),
            ("shell location", b"\nwindow.location.href;\n"),
            ("shell history", b"\nwindow.history.replaceState(null, '', '/');\n"),
            ("Synology token header", b'\nheaders["X-SYNO-TOKEN"] = "value";\n'),
            ("Synology token parameter", b'\nconst tokenName = "SynoToken";\n'),
        ):
            with self.subTest(name=name), self.assertRaisesRegex(
                validate_spk.ValidationError, "cookie-only AppWindow authentication"
            ):
                validate_build(api=source["api"] + marker)
        with self.assertRaisesRegex(validate_spk.ValidationError, "external network endpoint"):
            validate_build(api=source["api"] + b'\nfetch("https://evil.invalid/");\n')
        with self.assertRaisesRegex(validate_spk.ValidationError, "AppWindow cancellation"):
            validate_build(
                api=source["api"].replace(
                    b"signal: auth && auth.signal ? auth.signal : undefined,",
                    b"signal: undefined,",
                    1,
                )
            )
        with self.assertRaisesRegex(
            validate_spk.ValidationError, "queued-result observer|terminal horizon"
        ):
            validate_build(
                api=source["api"].replace(
                    b"for (;;)",
                    b"for (let attempt = 0; attempt < 60; attempt += 1)",
                    1,
                )
            )
        with self.assertRaisesRegex(validate_spk.ValidationError, "retry pending and transport"):
            validate_build(
                api=source["api"].replace(
                    b"await delay(interval, auth && auth.signal);",
                    b"await Promise.resolve();",
                    1,
                )
            )
        with self.assertRaisesRegex(validate_spk.ValidationError, "outcome-unknown"):
            validate_build(
                api=source["api"].replace(
                    b"throw new QueuedOutcomeUnknownError(",
                    b"throw new Error(",
                    1,
                )
            )
        with self.assertRaisesRegex(validate_spk.ValidationError, "resultOutput"):
            validate_build(
                api=source["api"].replace(
                    b"failure.resultOutput = boundedText(",
                    b"failure.resultText = boundedText(",
                    1,
                )
            )
        for name, original, replacement, pattern in (
            (
                "toast root",
                b'<div class="sdsync-toasts"',
                b'<div class="detached-toasts"',
                "root, toast host, and modal host",
            ),
            (
                "narrow navigation label",
                b':aria-label="item.title"',
                b':data-label="item.title"',
                "interaction contract",
            ),
            (
                "dialog listener cleanup",
                b'document.removeEventListener("keydown", this.confirmationKeyHandler, true);',
                b'document.removeEventListener("keydown", this.confirmationKeyHandler);',
                "interaction contract",
            ),
            (
                "secret route cleanup",
                b'if (this.route === "profiles" && route !== "profiles") this.closeProfile();',
                b'if (false) this.closeProfile();',
                "interaction contract",
            ),
            (
                "global mutation guard",
                b"openProfile(name) {\n      if (this.operationBusy) return;",
                b"openProfile(name) {",
                "operationBusy guard",
            ),
            (
                "Doctor terminal observation",
                b'const awaitTerminal = kind === "doctor";',
                b"const awaitTerminal = false;",
                "Doctor must terminal-poll",
            ),
        ):
            self.assertIn(original, source["app"], name)
            with self.subTest(name=name), self.assertRaisesRegex(
                validate_spk.ValidationError, pattern
            ):
                validate_build(app=source["app"].replace(original, replacement, 1))
        with self.assertRaisesRegex(validate_spk.ValidationError, "externalize Vue"):
            validate_build(
                webpack=source["webpack"].replace(b'vue: "Vue"', b'vue: "BundledVue"', 1)
            )

        script = (UI_SOURCE / "dist/SynologyDriveSync.js").read_bytes()
        style = (UI_SOURCE / "dist/style.css").read_bytes()
        for name, suffix, pattern in (
            ("eval", b"\neval('bad')", "forbidden runtime"),
            ("source map", b"\n//# sourceMappingURL=bad.map", "forbidden runtime"),
            ("bundled Vue", b'\nversion:"2.7.14"', "forbidden runtime"),
            ("remote endpoint", b'\nfetch("https://evil.invalid/")', "external network endpoint"),
        ):
            with self.subTest(name=name), self.assertRaisesRegex(
                validate_spk.ValidationError, pattern
            ):
                validate_spk.validate_native_bundle(script + suffix, style)
        with self.assertRaisesRegex(validate_spk.ValidationError, "not isolated"):
            validate_spk.validate_native_bundle(script, b":root { color: red; }\n" + style)
        with self.assertRaisesRegex(validate_spk.ValidationError, "remote asset"):
            validate_spk.validate_native_bundle(
                script, style + b'\n.sdsync-app { background: url("https://evil.invalid/x"); }'
            )

        installed_payload = build_spk.native_ui_payloads()[0][0]
        config = json.loads(installed_payload)
        duplicate_all_users = installed_payload.replace(
            b'"allUsers": false',
            b'"allUsers": true, "allUsers": false',
            1,
        )
        with self.assertRaisesRegex(validate_spk.ValidationError, "duplicate JSON key"):
            validate_spk.validate_ui_config(duplicate_all_users)
        config["SynologyDriveSync.js"][validate_spk.APP_ID]["allUsers"] = True
        with self.assertRaisesRegex(validate_spk.ValidationError, "allUsers"):
            validate_spk.validate_ui_config(json.dumps(config).encode())
        config = json.loads(installed_payload)
        del config["SynologyDriveSync.js"][validate_spk.APP_ID]["depend"]
        with self.assertRaisesRegex(validate_spk.ValidationError, "dependency list"):
            validate_spk.validate_ui_config(json.dumps(config).encode())
        with self.assertRaisesRegex(validate_spk.ValidationError, "one reviewed native"):
            validate_spk.validate_ui_config(
                json.dumps({".url": {validate_spk.APP_ID: {"type": "url"}}}).encode()
            )
        strings = (UI / "texts/enu/strings").read_bytes()
        validate_spk.validate_ui_texts(strings)
        with self.assertRaisesRegex(validate_spk.ValidationError, "fixed"):
            validate_spk.validate_ui_texts(
                strings.replace(
                    b"A configured Drive Sync profile failed.",
                    b"Profile %PROFILE% failed.",
                    1,
                )
            )
        with self.assertRaisesRegex(validate_spk.ValidationError, "reviewed sections"):
            validate_spk.validate_ui_texts(
                strings + b'\n[unexpected]\nextra="value"\n'
            )
        with self.assertRaisesRegex(validate_spk.ValidationError, "duplicate key"):
            validate_spk.validate_ui_texts(
                strings.replace(
                    b'sync_failed_title="Drive Sync failed"',
                    b'sync_failed_title="one"\nsync_failed_title="two"',
                    1,
                )
            )
        notifier = (HERE / "package/libexec/sdsync-common").read_bytes()
        with self.assertRaisesRegex(validate_spk.ValidationError, "synonotify"):
            validate_spk.validate_notifier(
                notifier
                + b'\n/usr/syno/bin/synonotify sync_failed '
                + b"'{\"%PROFILE%\":\"$notify_profile\"}'\n"
            )
        with self.assertRaisesRegex(validate_spk.ValidationError, "fixed reviewed argv"):
            validate_spk.validate_notifier(
                notifier.replace(
                    b"synology-drive-sync:notifications:sync_failed_message",
                    b'"$notify_profile"',
                    1,
                )
            )
        privilege = json.loads((HERE / "conf/privilege").read_text(encoding="utf-8"))
        validate_spk.validate_privilege(json.dumps(privilege).encode())
        duplicate_privilege = (
            b'{"defaults":{"run-as":"root"},'
            b'"defaults":{"run-as":"package"}}'
        )
        with self.assertRaisesRegex(validate_spk.ValidationError, "duplicate JSON key"):
            validate_spk.validate_privilege(duplicate_privilege)

        def privilege_model() -> dict[str, object]:
            return json.loads((HERE / "conf/privilege").read_text(encoding="utf-8"))

        root_run = privilege_model()
        root_run["defaults"] = {"run-as": "root"}
        with self.assertRaisesRegex(validate_spk.ValidationError, "root identity"):
            validate_spk.validate_privilege(json.dumps(root_run).encode())

        capabilities = privilege_model()
        capabilities["capabilities"] = ["CAP_SETUID"]
        with self.assertRaisesRegex(validate_spk.ValidationError, "Linux capabilities"):
            validate_spk.validate_privilege(json.dumps(capabilities).encode())

        joined_group = privilege_model()
        joined_group["join-groupname"] = "http"
        with self.assertRaisesRegex(validate_spk.ValidationError, "package-identity contract"):
            validate_spk.validate_privilege(json.dumps(joined_group).encode())

        setuid_tool = privilege_model()
        setuid_tool["tool"] = [{
            "relpath": "ui/api.cgi", "user": "package", "group": "package",
            "permission": "4755",
        }]
        with self.assertRaisesRegex(validate_spk.ValidationError, "setuid/setgid"):
            validate_spk.validate_privilege(json.dumps(setuid_tool).encode())

        ordinary_tool = privilege_model()
        ordinary_tool["tool"] = [{
            "relpath": "ui/api.cgi", "user": "package", "group": "package",
            "permission": "0755",
        }]
        with self.assertRaisesRegex(validate_spk.ValidationError, "package-identity contract"):
            validate_spk.validate_privilege(json.dumps(ordinary_tool).encode())

    @staticmethod
    def _alpha_bounds(payload: bytes, size: int) -> tuple[int, int, int, int]:
        offset = 8
        compressed = bytearray()
        while offset < len(payload):
            length = struct.unpack(">I", payload[offset : offset + 4])[0]
            kind = payload[offset + 4 : offset + 8]
            content = payload[offset + 8 : offset + 8 + length]
            offset += 12 + length
            if kind == b"IDAT":
                compressed.extend(content)
        raw = zlib.decompress(bytes(compressed))
        points = []
        for y in range(size):
            row = raw[y * (1 + 4 * size) : (y + 1) * (1 + 4 * size)]
            if row[0] != 0:
                raise AssertionError("icon generator unexpectedly used a PNG row filter")
            for x in range(size):
                if row[1 + x * 4 + 3]:
                    points.append((x, y))
        return (
            min(point[0] for point in points),
            max(point[0] for point in points),
            min(point[1] for point in points),
            max(point[1] for point in points),
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
