#!/usr/bin/env python3
"""Focused contract tests for the offline DSM desktop application and SPK UI."""

from __future__ import annotations

import hashlib
import io
import json
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zlib
from pathlib import Path


HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import build_spk  # noqa: E402
import validate_spk  # noqa: E402


UI = HERE / "package/ui"


class DsmUiContractTests(unittest.TestCase):
    def test_source_validator_covers_rootless_ui_and_dsm_bounds(self) -> None:
        validate_spk.validate_source()
        info = (HERE / "INFO.template").read_text(encoding="utf-8")
        self.assertIn('os_min_ver="7.0-40759"', info)
        self.assertIn('os_max_ver="7.4-99999"', info)
        self.assertIn('dsmuidir="ui"', info)
        self.assertIn('dsmappname="com.supermarsx.SynologyDriveSync"', info)

    def test_dsm_application_and_direct_notification_contract_match(self) -> None:
        config = json.loads((UI / "config").read_text(encoding="utf-8"))
        notifier = (HERE / "package/libexec/sdsync-common").read_bytes()
        app_id = "com.supermarsx.SynologyDriveSync"
        application = config[".url"][app_id]
        self.assertEqual(application["type"], "url")
        self.assertEqual(application["url"], "3rdparty/synology-drive-sync/index.html")
        self.assertIs(application["allUsers"], False)
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

    def test_ui_is_dark_first_accessible_responsive_and_offline(self) -> None:
        index = (UI / "index.html").read_text(encoding="utf-8")
        css = (UI / "app.css").read_text(encoding="utf-8")
        script = (UI / "app.js").read_text(encoding="utf-8")
        self.assertIn('<html lang="en" data-theme="dark">', index)
        self.assertLess(index.index('<meta name="referrer" content="no-referrer">'), index.index("<link"))
        for route in ("overview", "profiles", "routines", "health", "activity", "notifications", "settings"):
            self.assertEqual(index.count(f'data-route="{route}"'), 1)
            self.assertEqual(index.count(f'data-page="{route}"'), 1)
        for marker in (
            'aria-live="polite"', 'aria-labelledby="health-title"', 'data-confirm-dialog',
            'data-target-health', 'data-routine-form', 'data-alert-policy-form',
            'data-invalid-cert-warning', 'data-remote-log-token-input',
        ):
            self.assertIn(marker, index)
        self.assertIn("@media (max-width: 840px)", css)
        self.assertIn("@media (prefers-reduced-motion: reduce)", css)
        self.assertIn(":focus-visible", css)
        self.assertNotRegex(index, r"\son[a-z]+\s*=")
        self.assertNotIn(".innerHTML", script)
        self.assertNotIn("eval(", script)
        validate_spk.validate_ui_static(index.encode(), css.encode(), script.encode())

    def test_advanced_profile_and_secret_semantics_are_explicit(self) -> None:
        index = (UI / "index.html").read_text(encoding="utf-8")
        script = (UI / "app.js").read_text(encoding="utf-8")
        manager = (HERE / "package/bin/sdsync-dsm").read_text(encoding="utf-8")
        for field in (
            "excludes", "allow_empty_source", "retries", "timeout", "connect_timeout",
            "max_rate", "ca_certificate", "danger_invalid_certs", "verbosity", "quiet",
            "log_level", "log_format", "log_file", "progress", "output",
            "remote_log_url", "remote_log_mode", "remote_log_token_mode",
        ):
            self.assertIn(f'name="{field}"', index)
        for secret in ("password", "totp", "remote_log_token"):
            self.assertRegex(index, rf'name="{secret}_mode"[^>]*>.*?value="keep".*?value="replace".*?value="clear"')
        for managed in ("log_format", "log_file", "progress", "output"):
            self.assertRegex(index, rf'name="{managed}"[^>]*data-managed[^>]*disabled')
        self.assertRegex(index, r'name="connect_timeout"[^>]*min="1"[^>]*max="600"')
        self.assertIn("form.elements.name.readOnly = Boolean(profile)", script)
        self.assertLess(script.index("if (risky && !await confirmAction"), script.index("clearSecretInputs();", script.index("async function saveProfile")))
        self.assertNotRegex(script, r"localStorage[^\n]*(?:password|totp|remote_log_token)")
        quiet_guard = script.index("if (form.elements.quiet.checked && Number(form.elements.verbosity.value) !== 0)")
        self.assertLess(quiet_guard, script.index("const profile = collectProfile(form);", quiet_guard))
        self.assertIn("Quiet terminal output cannot be combined with verbose output", script)
        self.assertIn("set-remote-log-token NAME [--from-file FILE]", manager)
        self.assertIn("remove-password NAME | remove-totp NAME | remove-remote-log-token NAME", manager)
        self.assertIn("--kind password|totp|remote-log-token --mode replace|clear", manager)

    def test_routine_health_and_notification_surfaces_are_complete(self) -> None:
        index = (UI / "index.html").read_text(encoding="utf-8")
        for field in (
            "profile", "enabled", "action", "mode", "interval_seconds", "window_start",
            "window_end", "debounce_seconds", "poll_seconds", "retry_count",
            "retry_backoff_seconds", "depends_on", "allow_delete", "max_total_delete",
        ):
            self.assertIn(f'name="{field}"', index)
        for value in ("interval", "daily", "realtime"):
            self.assertIn(f'value="{value}"', index)
        for column in ("Reachable", "Auth", "Writable", "Latency", "Last success", "Doctor", "Free space"):
            self.assertIn(f">{column}<", index)
        self.assertIn("Free space is shown only when the backend proves it", index)
        for field in ("on_success", "on_failure", "failure_threshold", "cooldown_seconds"):
            self.assertIn(f'name="{field}"', index)
        self.assertRegex(index, r'name="cooldown_seconds"[^>]*min="60"[^>]*max="604800"')
        for event in ("sync_succeeded", "sync_failed", "doctor_failed"):
            self.assertIn(f"<code>{event}</code>", index)

    def test_browser_bridge_actions_and_argument_keys_are_exact(self) -> None:
        script = (UI / "app.js").read_text(encoding="utf-8")
        get_match = re.search(r'const GET_ACTIONS = Object\.freeze\((\[[^;]+\])\);', script)
        self.assertIsNotNone(get_match)
        self.assertEqual(json.loads(get_match.group(1)), ["csrf", "snapshot", "logs", "activity", "result"])
        expected = {
            "configure-profile": ["allow_empty_source", "allow_http", "ca_certificate", "compare", "connect_timeout_seconds", "danger_accept_invalid_certs", "delete", "excludes", "jobs", "log_level", "make_default", "max_delete", "max_rate_bytes_per_second", "name", "quiet", "remote", "remote_log_mode", "remote_log_url", "retries", "source", "timeout_seconds", "url", "username", "verbosity"],
            "remove-profile": ["name"],
            "set-default": ["name"],
            "set-secret": ["kind", "mode", "profile", "value"],
            "schedule": ["allow_delete", "enabled", "interval_seconds", "max_total_delete"],
            "routine": ["action", "allow_delete", "debounce_seconds", "depends_on", "enabled", "interval_seconds", "max_total_delete", "mode", "poll_seconds", "profile", "retry_backoff_seconds", "retry_count", "time_window_end", "time_window_start", "weekdays"],
            "remove-routine": ["name"],
            "alert-policy": ["cooldown_seconds", "enabled", "failure_threshold", "on_failure", "on_success"],
            "action": ["allow_delete", "kind", "max_total_delete", "scope", "write_test"],
        }
        actual = {}
        for operation, values in re.findall(r'^    "([a-z-]+)": Object\.freeze\((\[[^\n]+\])\)', script, re.MULTILINE):
            actual[operation] = json.loads(values)
        self.assertEqual(actual, expected)
        for marker in (
            "crypto.getRandomValues(random)", "request_id: requestId", "operation: action",
            "arguments: payload", '"X-SYNO-TOKEN": state.synoToken',
            '"X-SDSYNC-CSRF": state.csrfToken', "token.length > 1024",
            'result: Object.freeze(["job_id"])', 'const RESULT_STATUS_SCHEMA = "sdsync.dsm-result-status.v1"',
            "pollJobResult(queued.job_id)", "awaitTerminal === false",
            'apiGet("result", { job_id: jobId })', "/^[0-9a-f]{48}$/",
            'response.status === 410', 'status.state === "expired_or_missing"',
            'status.result.code !== "expired_or_missing"',
        ):
            self.assertIn(marker, script)
        self.assertRegex(script, r'apiPost\(ACTIONS\.execute,[\s\S]*?, false\)')

    def test_launch_token_run_details_and_zero_routine_values_are_preserved(self) -> None:
        index = (UI / "index.html").read_text(encoding="utf-8")
        script = (UI / "app.js").read_text(encoding="utf-8")

        token_function = script[
            script.index("  function consumeLaunchToken()") : script.index("\n  function one(")
        ]
        validation = token_function.index("if (!token || token.length > 1024")
        self.assertLess(token_function.index('url.searchParams.delete("SynoToken")'), validation)
        self.assertLess(token_function.index('url.searchParams.delete("synotoken")'), validation)
        self.assertLess(token_function.index("window.history.replaceState"), validation)

        details = re.search(r'<dl class="definition-grid" data-run-details>(.*?)</dl>', index, re.DOTALL)
        self.assertIsNotNone(details)
        self.assertEqual(
            re.findall(r"<dt>([^<]+)</dt>", details.group(1)),
            ["Operation", "State", "Scope", "Started", "Finished"],
        )
        self.assertIn(
            'const values = [boundedText(run.operation, "Unavailable"), status, scope, '
            "formatDate(run.started_epoch), formatDate(run.finished_epoch)];",
            script,
        )

        self.assertIn(
            'setFormValue(form, "retry_count", definedOr(pick(routine, "retry_count"), 2));',
            script,
        )
        self.assertIn(
            'setFormValue(form, "max_total_delete", definedOr(pick(routine, "max_total_delete"), 100));',
            script,
        )
        self.assertIn(
            "return value === undefined || value === null ? fallback : value;",
            script,
        )

    def test_javascript_parses_when_node_is_available(self) -> None:
        node = shutil.which("node")
        if not node:
            self.skipTest("node is not installed")
        result = subprocess.run([node, "--check", str(UI / "app.js")], capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_icon_family_is_deterministic_snapshotted_and_inside_safe_bounds(self) -> None:
        expected_hashes = {
            16: "53b66efdee04cf34b84599568bc7eba241c55bfebe3f80e4ac08123e766a0c15",
            24: "1da35a323aa6603100cf05fb7067d9679732a2dabe2336af96c29013f6baa12b",
            32: "ad9229bedca146b48f6d8de0a88af921a77e8857ab9527a8dc7d71ac68fa6e32",
            48: "81acf1abc7f703d7191623c00338dc224966f659c39b068c3a848fde08f4d08b",
            64: "579a2d4bf06650dc72326eb12c3d500b153fcab66aa0dac23c3be40c202c39d0",
            72: "cef7ba027c33e8bd66ad015590c24a3980255b2d149a2f4052f4b44ecb5e6edd",
            256: "e2549bfe7b81a8650bb2ae17a297fcd9e0ceb50d1c67f8000bec8c9200c7733e",
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
            for name in ("ui/config", "ui/index.html", "ui/app.css", "ui/app.js", "ui/images/icon.svg", "ui/texts/enu/strings"):
                self.assertIn(name, members)
            self.assertNotIn("ui/texts/enu/mails", members)
            self.assertEqual(installed_size, sum(member.size for member in members.values() if member.isfile()))

    def test_builder_refuses_setid_archive_metadata(self) -> None:
        with self.assertRaisesRegex(build_spk.PackageError, "must not carry setuid/setgid"):
            build_spk.tar_info("ui/api.cgi", 0o4755, 1)

    def test_static_validators_reject_security_and_registration_tampering(self) -> None:
        index = (UI / "index.html").read_bytes()
        css = (UI / "app.css").read_bytes()
        script = (UI / "app.js").read_bytes()
        with self.assertRaisesRegex(validate_spk.ValidationError, "inline event"):
            validate_spk.validate_ui_static(index.replace(b"<body>", b'<body onload="steal()">'), css, script)
        with self.assertRaisesRegex(validate_spk.ValidationError, "external network endpoint"):
            validate_spk.validate_ui_static(index, css, script + b'\nfetch("https://evil.invalid/");\n')
        with self.assertRaisesRegex(validate_spk.ValidationError, "persists password"):
            validate_spk.validate_ui_static(index, css, script + b'\nlocalStorage.setItem("password", "bad");\n')
        with self.assertRaisesRegex(validate_spk.ValidationError, "result-status"):
            validate_spk.validate_ui_static(
                index,
                css,
                script.replace(b"sdsync.dsm-result-status.v1", b"sdsync.dsm-unknown.v1"),
            )
        config = json.loads((UI / "config").read_text(encoding="utf-8"))
        duplicate_all_users = json.dumps(config).encode().replace(
            b'"allUsers": false',
            b'"allUsers": true, "allUsers": false',
            1,
        )
        with self.assertRaisesRegex(validate_spk.ValidationError, "duplicate JSON key"):
            validate_spk.validate_ui_config(duplicate_all_users)
        config[".url"]["com.supermarsx.SynologyDriveSync"]["allUsers"] = True
        with self.assertRaisesRegex(validate_spk.ValidationError, "allUsers"):
            validate_spk.validate_ui_config(json.dumps(config).encode())
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
            b'"defaults":{"run-as":"package"},"join-groupname":"http"}'
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

        wrong_group = privilege_model()
        wrong_group["join-groupname"] = "package"
        with self.assertRaisesRegex(validate_spk.ValidationError, "rootless package/http"):
            validate_spk.validate_privilege(json.dumps(wrong_group).encode())

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
        with self.assertRaisesRegex(validate_spk.ValidationError, "rootless package/http"):
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
