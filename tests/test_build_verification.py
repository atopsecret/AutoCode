"""验证器单元测试：所有 App、Mach-O 与 codesign 输出均为合成或 mock，不代表构建通过。"""
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import plistlib
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/verify-build.py"
SPEC = importlib.util.spec_from_file_location("build_verification", SCRIPT)
VERIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFIER)


class SyntheticCommands:
    def __init__(self):
        self.calls = []
        self.outputs = {
            "file": b"Mach-O 64-bit executable arm64\n",
            "lipo": b"arm64\n",
            "dyld": b"",
            "verify": b"",
            "display": b"Identifier=com.autocode.app\nSignature=adhoc\n",
            "entitlements": plistlib.dumps({"com.apple.security.automation.apple-events": True}),
        }
        self.returncodes = {}
        self.raise_timeout = False

    def __call__(self, command, **kwargs):
        self.calls.append(command)
        if self.raise_timeout:
            raise subprocess.TimeoutExpired(command, kwargs["timeout"])
        if command[0].endswith("file"):
            name = "file"
        elif command[0].endswith("lipo"):
            name = "lipo"
        elif "dyld_info" in command:
            name = "dyld"
        elif "--verify" in command:
            name = "verify"
        elif "--entitlements" in command:
            name = "entitlements"
        else:
            name = "display"
        return subprocess.CompletedProcess(command, self.returncodes.get(name, 0), self.outputs[name], b"")


class BuildVerificationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="autocode-build-verification-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "source"
        self.root.mkdir()
        self.commands = SyntheticCommands()
        self.environment = mock.patch.dict(VERIFIER.os.environ, {"CARGO_TARGET_DIR": str(self.root / "src-tauri/target")})
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.host = mock.patch.object(VERIFIER.platform, "system", return_value="Darwin")
        self.machine = mock.patch.object(VERIFIER.platform, "machine", return_value="arm64")
        self.host.start()
        self.machine.start()
        self.addCleanup(self.host.stop)
        self.addCleanup(self.machine.stop)
        self.write_json("package.json", {"name": "autocode", "version": "0.6.0"})
        self.write_json("package-lock.json", {"name": "autocode", "version": "0.6.0", "packages": {"": {"name": "autocode", "version": "0.6.0"}}})
        self.write("src-tauri/Cargo.toml", '[package]\nname = "autocode"\nversion = "0.6.0"\n')
        self.write("src-tauri/Cargo.lock", 'version = 4\n[[package]]\nname = "autocode"\nversion = "0.6.0"\n')
        self.config = {
            "version": "0.6.0", "identifier": "com.autocode.app", "productName": "AutoCode",
            "build": {"frontendDist": "../src"},
            "app": {"windows": [{"label": "main"}]},
            "bundle": {"icon": ["icons/icon.icns"], "macOS": {"minimumSystemVersion": "12.0", "entitlements": "Entitlements.plist"}},
        }
        self.write_json("src-tauri/tauri.conf.json", self.config)
        self.write("src/index.html", '<link rel="stylesheet" href="styles.css"><script type="module" src="main.js"></script>')
        self.write("src/popup.html", '<link rel="stylesheet" href="popup.css"><script type="module" src="popup.js"></script>')
        self.write("src/main.js", "import { latest } from './ui-state.js';\n")
        self.write("src/popup.js", "import './ui-state.js';\n")
        self.write("src/ui-state.js", "export const latest = null;\n")
        self.write("src/styles.css", "body { margin: 0; }\n")
        self.write("src/popup.css", "body { margin: 0; }\n")
        self.write("src-tauri/icons/icon.icns", "synthetic icon")
        self.write("src-tauri/Entitlements.plist", plistlib.dumps({"com.apple.security.automation.apple-events": True}))
        self.app = self.root / "src-tauri/target/release/bundle/macos/AutoCode.app"

    def write(self, relative, content):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(content, bytes):
            path.write_bytes(content)
        else:
            path.write_text(content, encoding="utf-8")
        return path

    def write_json(self, relative, value):
        return self.write(relative, json.dumps(value))

    def make_app(self, **overrides):
        info = {"CFBundleShortVersionString": "0.6.0", "CFBundleVersion": "0.6.0",
                "CFBundleIdentifier": "com.autocode.app", "CFBundleExecutable": "autocode",
                "LSMinimumSystemVersion": "12.0", "LSUIElement": True, "CFBundleIconFile": "icon.icns"}
        info.update(overrides)
        path = self.app / "Contents/Info.plist"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(plistlib.dumps(info))
        executable = self.app / "Contents/MacOS/autocode"
        executable.parent.mkdir()
        executable.write_bytes(b"synthetic executable; external inspection is mocked")
        executable.chmod(0o755)
        icon = self.app / "Contents/Resources/icon.icns"
        icon.parent.mkdir()
        icon.write_bytes(b"synthetic bundle icon")
        return executable

    def source_report(self):
        return VERIFIER.verify(self.root, source_only=True, runner=self.commands)

    def app_report(self):
        return VERIFIER.verify(self.root, app=self.app, runner=self.commands)

    def test_legal_semver(self):
        for version in ["0.6.0", "0.0.0", "1.2.3-alpha.1+build.007", "1.2.3-0", "1.2.3-rc-01"]:
            self.assertTrue(VERIFIER.valid_semver(version), version)

    def test_invalid_semver(self):
        for version in ["01.2.3", "1.02.3", "1.2", "v1.2.3", "1.2.3-01", "1.2.3+", "1.2.3-", "1.2.3 alpha", 123, None]:
            self.assertFalse(VERIFIER.valid_semver(version), version)

    def test_source_six_versions_and_resources_pass_without_commands(self):
        report = self.source_report()
        self.assertTrue(report["passed"])
        versions = next(check["versions"] for check in report["checks"] if check["name"] == "source_versions")
        self.assertEqual(len(versions), 6)
        self.assertFalse(report["source_only_is_build_success"])
        self.assertFalse(report["app_executed"])
        self.assertEqual(self.commands.calls, [])

    def test_lock_root_version_mismatch_fails(self):
        lock = json.loads((self.root / "package-lock.json").read_text())
        lock["packages"][""]["version"] = "0.5.0"
        self.write_json("package-lock.json", lock)
        self.assertFalse(self.source_report()["passed"])

    def test_cargo_lock_duplicate_autocode_fails(self):
        with (self.root / "src-tauri/Cargo.lock").open("a") as stream:
            stream.write('[[package]]\nname="autocode"\nversion="0.5.0"\n')
        self.assertFalse(self.source_report()["passed"])

    def test_missing_popup_entry_fails(self):
        (self.root / "src/popup.html").unlink()
        self.assertFalse(self.source_report()["passed"])

    def test_missing_stylesheet_fails(self):
        (self.root / "src/popup.css").unlink()
        self.assertFalse(self.source_report()["passed"])

    def test_missing_imported_module_fails(self):
        (self.root / "src/ui-state.js").unlink()
        self.assertFalse(self.source_report()["passed"])

    def test_missing_icon_fails(self):
        (self.root / "src-tauri/icons/icon.icns").unlink()
        self.assertFalse(self.source_report()["passed"])

    def test_resource_escape_fails(self):
        self.write("src/main.js", "import '../../private.js';\n")
        self.assertFalse(self.source_report()["passed"])

    def test_external_stylesheet_fails(self):
        self.write("src/popup.html", '<link rel="stylesheet" href="https://example.invalid/style.css">')
        self.assertFalse(self.source_report()["passed"])

    def test_invalid_json_is_reported(self):
        self.write("package.json", "{")
        self.assertFalse(self.source_report()["passed"])

    def test_invalid_nested_structure_is_reported(self):
        self.write_json("src-tauri/tauri.conf.json", {"version": "0.6.0", "build": 1})
        self.assertFalse(self.source_report()["passed"])

    def test_source_info_requires_accessory_flag(self):
        self.config["bundle"]["macOS"]["infoPlist"] = "Info.plist"
        self.write_json("src-tauri/tauri.conf.json", self.config)
        self.write("src-tauri/Info.plist", plistlib.dumps({"LSUIElement": False}))
        self.assertFalse(self.source_report()["passed"])

    def test_invalid_source_entitlements_are_reported(self):
        self.write("src-tauri/Entitlements.plist", b"<plist><invalid")
        self.assertFalse(self.source_report()["passed"])

    def test_source_pass_without_app_is_not_artifact_pass(self):
        report = VERIFIER.verify(self.root, runner=self.commands)
        self.assertFalse(report["passed"])
        self.assertEqual(self.commands.calls, [])

    def test_missing_built_app_fails(self):
        self.assertFalse(self.app_report()["passed"])
        self.assertEqual(self.commands.calls, [])

    def test_installed_app_is_not_a_build_output(self):
        report = VERIFIER.verify(self.root, app=Path("/Applications/AutoCode.app"), runner=self.commands)
        self.assertFalse(report["passed"])
        self.assertEqual(self.commands.calls, [])

    def test_synthetic_app_static_contract_with_mock_commands(self):
        executable = self.make_app()
        report = self.app_report()
        self.assertTrue(report["passed"])
        self.assertEqual(report["executable_sha256"], hashlib.sha256(executable.read_bytes()).hexdigest())
        self.assertEqual(report["signature"]["Signature"], ["adhoc"])
        self.assertTrue(report["entitlements"]["com.apple.security.automation.apple-events"])
        self.assertFalse(report["app_executed"])
        self.assertEqual(len(self.commands.calls), 6)
        self.assertTrue(all(command[0] in ["/usr/bin/file", "/usr/bin/lipo", "/usr/bin/xcrun", "/usr/bin/codesign"] for command in self.commands.calls))

    def test_malformed_loader_layout_fails_even_when_codesign_passes(self):
        self.make_app()
        self.commands.returncodes["dyld"] = 1
        self.commands.outputs["dyld"] = b"mis-aligned LINKEDIT string pool\n"
        report = self.app_report()
        self.assertFalse(report["passed"])
        checks = {check["name"]: check for check in report["checks"]}
        self.assertFalse(checks["dyld_validate"]["passed"])
        self.assertTrue(checks["codesign_verify"]["passed"])
        self.assertIn("mis-aligned LINKEDIT", checks["dyld_validate"]["stdout"])

    def test_missing_loader_validation_tool_fails_closed(self):
        self.make_app()
        commands = self.commands

        def without_dyld(command, **kwargs):
            if "dyld_info" in command:
                raise FileNotFoundError("dyld_info unavailable")
            return commands(command, **kwargs)

        report = VERIFIER.verify(self.root, app=self.app, runner=without_dyld)
        self.assertFalse(report["passed"])
        self.assertFalse(next(check for check in report["checks"] if check["name"] == "dyld_validate")["passed"])

    def test_old_bundle_version_fails(self):
        self.make_app(CFBundleShortVersionString="0.5.0")
        self.assertFalse(self.app_report()["passed"])

    def test_wrong_build_version_fails(self):
        self.make_app(CFBundleVersion="0.5.0")
        self.assertFalse(self.app_report()["passed"])

    def test_wrong_identifier_fails(self):
        self.make_app(CFBundleIdentifier="com.other.app")
        self.assertFalse(self.app_report()["passed"])

    def test_wrong_minimum_system_fails(self):
        self.make_app(LSMinimumSystemVersion="10.13")
        self.assertFalse(self.app_report()["passed"])

    def test_non_accessory_bundle_fails(self):
        self.make_app(LSUIElement=False)
        self.assertFalse(self.app_report()["passed"])

    def test_missing_bundle_icon_fails(self):
        self.make_app()
        (self.app / "Contents/Resources/icon.icns").unlink()
        self.assertFalse(self.app_report()["passed"])

    def test_bundle_icon_without_extension_resolves_icns(self):
        self.make_app(CFBundleIconFile="icon")
        self.assertTrue(self.app_report()["passed"])

    def test_bundle_icon_cannot_escape_resources(self):
        self.make_app(CFBundleIconFile="../../outside.icns")
        self.assertFalse(self.app_report()["passed"])

    def test_wrong_executable_name_fails(self):
        self.make_app(CFBundleExecutable="old-code")
        self.assertFalse(self.app_report()["passed"])

    def test_missing_executable_fails(self):
        executable = self.make_app()
        executable.unlink()
        self.assertFalse(self.app_report()["passed"])

    def test_non_executable_file_fails(self):
        executable = self.make_app()
        executable.chmod(0o644)
        self.assertFalse(self.app_report()["passed"])

    def test_bad_plist_is_reported(self):
        self.make_app()
        (self.app / "Contents/Info.plist").write_bytes(b"<plist><invalid")
        self.assertFalse(self.app_report()["passed"])

    def test_non_mach_o_fails(self):
        self.make_app()
        self.commands.outputs["file"] = b"ASCII text\n"
        self.assertFalse(self.app_report()["passed"])

    def test_wrong_architecture_fails(self):
        self.make_app()
        self.commands.outputs["lipo"] = b"x86_64\n"
        self.assertFalse(self.app_report()["passed"])

    def test_universal_native_architecture_passes(self):
        self.make_app()
        self.commands.outputs["lipo"] = b"x86_64 arm64\n"
        self.assertTrue(self.app_report()["passed"])

    def test_codesign_strict_failure_fails(self):
        self.make_app()
        self.commands.returncodes["verify"] = 1
        report = self.app_report()
        self.assertFalse(report["passed"])
        self.assertTrue(any("--strict" in command and "--deep" in command for command in self.commands.calls))

    def test_signature_identifier_mismatch_fails(self):
        self.make_app()
        self.commands.outputs["display"] = b"Identifier=com.other.app\nSignature=adhoc\n"
        self.assertFalse(self.app_report()["passed"])

    def test_missing_entitlement_fails(self):
        self.make_app()
        self.commands.outputs["entitlements"] = plistlib.dumps({})
        self.assertFalse(self.app_report()["passed"])

    def test_malformed_entitlement_is_reported(self):
        self.make_app()
        self.commands.outputs["entitlements"] = b"not a plist"
        self.assertFalse(self.app_report()["passed"])

    def test_external_command_timeout_is_reported(self):
        self.make_app()
        self.commands.raise_timeout = True
        self.assertFalse(self.app_report()["passed"])

    def test_non_macos_host_cannot_validate_app_signature(self):
        self.make_app()
        with mock.patch.object(VERIFIER.platform, "system", return_value="Linux"):
            self.assertFalse(self.app_report()["passed"])
        self.assertEqual(self.commands.calls, [])

    def test_cli_source_report_saved_and_exit_code_zero(self):
        report_path = Path(self.temporary.name) / "report.json"
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = VERIFIER.main(["--root", str(self.root), "--source-only", "--report", str(report_path)])
        self.assertEqual(result, 0)
        self.assertEqual(json.loads(output.getvalue()), json.loads(report_path.read_text()))
        self.assertEqual(json.loads(output.getvalue())["scope"], "source-only")

    def test_cli_missing_app_exit_code_nonzero(self):
        with contextlib.redirect_stdout(io.StringIO()):
            result = VERIFIER.main(["--root", str(self.root), "--app", str(self.app)])
        self.assertEqual(result, 1)

    def test_report_write_failure_is_nonzero(self):
        blocked = Path(self.temporary.name) / "file-not-directory"
        blocked.write_text("test")
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = VERIFIER.main(["--root", str(self.root), "--source-only", "--report", str(blocked / "report.json")])
        self.assertEqual(result, 1)
        self.assertFalse(json.loads(output.getvalue())["passed"])


if __name__ == "__main__":
    unittest.main()
