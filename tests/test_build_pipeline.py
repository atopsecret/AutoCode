"""构建入口回归；只运行合成子进程，不编译或启动 App。"""

import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import subprocess
import time
import tomllib
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("build_local", Path(__file__).parents[1] / "scripts/build-local.py")
BUILD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BUILD)


class BuildPipelineTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.directory = Path(self.tmp.name)

    def steps(self, offline):
        with patch.object(BUILD.platform, "machine", return_value="arm64"):
            return BUILD.build_steps(["cargo", "tauri"], offline, "app", 30, 60, self.directory / "app-report.json")

    def test_every_cargo_build_and_test_is_locked(self):
        for name, command, _ in self.steps(False):
            if command[0] == "cargo":
                self.assertIn("--locked", command, name)

    def test_release_artifacts_are_not_stripped(self):
        manifest = tomllib.loads((BUILD.ROOT / "src-tauri/Cargo.toml").read_text())
        release = manifest["profile"]["release"]
        self.assertEqual(release["strip"], "none")
        self.assertNotIn("opt-level", release)

    def test_offline_does_not_fetch(self):
        steps = self.steps(True)
        self.assertNotIn("dependencies", [name for name, _, _ in steps])
        for _, command, _ in steps:
            if command[0] == "cargo":
                self.assertIn("--offline", command)

    def test_online_fetch_is_host_targeted_and_bounded(self):
        fetch = next(step for step in self.steps(False) if step[0] == "dependencies")
        self.assertIn("aarch64-apple-darwin", fetch[1])
        self.assertEqual(fetch[2], 30)

    def test_bundle_precedes_verification_and_does_not_launch(self):
        steps = self.steps(True)
        names = [name for name, _, _ in steps]
        self.assertLess(names.index("full-tests"), names.index("release-bundle"))
        self.assertLess(names.index("release-bundle"), names.index("app-verification"))
        self.assertIn("--app", steps[-1][1])
        for _, command, _ in steps:
            self.assertNotIn(command[0], ("open", "osascript", "launchctl"))

    def test_success_records_log_and_exit_code(self):
        result = BUILD.run_step("synthetic-pass", [sys.executable, "-c", "print('synthetic')"], 5, self.directory, os.environ.copy())
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["exit_code"], 0)
        self.assertEqual(Path(result["log"]).read_text().strip(), "synthetic")

    def test_failure_is_not_reported_as_pass(self):
        result = BUILD.run_step("synthetic-fail", [sys.executable, "-c", "raise SystemExit(7)"], 5, self.directory, os.environ.copy())
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["exit_code"], 7)

    def test_timeout_is_reported_and_child_is_reaped(self):
        spawned = []
        original = subprocess.Popen

        def capture(*args, **kwargs):
            process = original(*args, **kwargs)
            spawned.append(process)
            return process

        with patch.object(BUILD.subprocess, "Popen", side_effect=capture):
            result = BUILD.run_step("synthetic-timeout", [sys.executable, "-c", "import time; time.sleep(20)"], 0.1, self.directory, os.environ.copy())
        self.assertEqual(result["status"], "timeout")
        self.assertEqual(result["exit_code"], 124)
        self.assertLess(result["elapsed_seconds"], 8)
        self.assertIsNotNone(spawned[0].returncode)
        self.assertEqual(spawned[0].wait(timeout=0.1), spawned[0].returncode)

    def assert_synthetic_child_stopped(self, heartbeat):
        # 只检查测试子进程自己的活动文件，不依赖 ps 或读取系统进程列表。
        before = heartbeat.read_text()
        time.sleep(0.15)
        self.assertEqual(heartbeat.read_text(), before, "合成子进程终止后仍在写入活动文件")

    def test_failed_parent_does_not_leave_child_ignoring_sigterm(self):
        marker = self.directory / "synthetic-child.pid"
        heartbeat = self.directory / "synthetic-child.activity"
        child = "\n".join([
            "import signal,time,os",
            "from pathlib import Path",
            "signal.signal(signal.SIGTERM, signal.SIG_IGN)",
            f"Path({str(heartbeat)!r}).write_text('0')",
            f"Path({str(marker)!r}).write_text(str(os.getpid()))",
            "for n in range(1000):",
            f"    Path({str(heartbeat)!r}).write_text(str(n)); time.sleep(0.02)",
        ])
        parent = "\n".join([
            "import subprocess,sys,time",
            "from pathlib import Path",
            f"subprocess.Popen([sys.executable, '-c', {child!r}])",
            f"while not Path({str(marker)!r}).exists(): time.sleep(0.01)",
            "raise SystemExit(7)",
        ])
        result = BUILD.run_step("synthetic-orphan", [sys.executable, "-c", parent], 5, self.directory, os.environ.copy())
        self.assertEqual(result["exit_code"], 7)
        self.assert_synthetic_child_stopped(heartbeat)

    def test_sigterm_cancels_pipeline_and_cleans_owned_stage(self):
        project = self.directory / "project"
        project.mkdir()
        (project / "package.json").write_text(json.dumps({"version": "0.6.0"}))
        marker = self.directory / "stage.pid"
        heartbeat = self.directory / "stage.activity"
        sleeping = "\n".join([
            "import os,time",
            "from pathlib import Path",
            f"Path({str(heartbeat)!r}).write_text('0')",
            f"Path({str(marker)!r}).write_text(str(os.getpid()))",
            "for n in range(1000):",
            f"    Path({str(heartbeat)!r}).write_text(str(n)); time.sleep(0.02)",
        ])
        code = "\n".join([
            "import importlib.util,sys",
            "from pathlib import Path",
            "from unittest.mock import patch",
            f"spec=importlib.util.spec_from_file_location('builder', {str(Path(BUILD.__file__).resolve())!r})",
            "m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m)",
            "original=m.run_step",
            f"def stage(name,command,timeout,directory,env): return original(name,[sys.executable,'-c',{sleeping!r}],timeout,directory,env)",
            f"with patch.object(m,'ROOT',Path({str(project)!r})), patch.object(m.sys,'platform','darwin'), patch.object(m.platform,'machine',return_value='arm64'), patch.object(m.shutil,'which',return_value='/synthetic/cargo-tauri'), patch.object(m,'run_step',side_effect=stage):",
            "    raise SystemExit(m.main(['--offline','--bundles','app']))",
        ])
        process = subprocess.Popen([sys.executable, "-c", code], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        self.addCleanup(BUILD.terminate_group, process)
        deadline = time.monotonic() + 5
        while not marker.exists() and time.monotonic() < deadline and process.poll() is None:
            time.sleep(0.01)
        self.assertTrue(marker.exists(), "合成构建阶段未启动")
        process.terminate()
        self.assertEqual(process.wait(timeout=8), 130)
        self.assert_synthetic_child_stopped(heartbeat)
        report = json.loads(next(project.glob("artifacts/build-0.6.0/*/build-report.json")).read_text())
        self.assertEqual(report["status"], "cancelled")
        self.assertFalse(report["full_application_build_passed"])

    def test_report_preserves_failure_and_does_not_claim_build(self):
        report = {"status": "failed", "full_application_build_passed": False}
        BUILD.write_report(self.directory, report)
        self.assertEqual(json.loads((self.directory / "build-report.json").read_text()), report)

    def run_synthetic_pipeline(self, failed_step=None, cli_version="2.10.1", offline=True):
        (self.directory / "package.json").write_text(json.dumps({"version": "0.6.0"}))
        (self.directory / "package-lock.json").write_text(json.dumps({
            "packages": {"node_modules/@tauri-apps/cli": {"version": "2.10.1"}}
        }))
        calls = []

        def synthetic_step(name, command, timeout, report_dir, env):
            calls.append(name)
            (report_dir / f"{name}.log").write_text(f"tauri-cli {cli_version}\n" if name == "cli-version" else "synthetic only\n")
            return {"name": name, "status": "failed" if name == failed_step else "passed", "exit_code": 1 if name == failed_step else 0}

        with patch.object(BUILD, "ROOT", self.directory), \
             patch.object(BUILD.sys, "platform", "darwin"), \
             patch.object(BUILD.platform, "machine", return_value="arm64"), \
             patch.object(BUILD.shutil, "which", return_value="/synthetic/cargo-tauri"), \
             patch.object(BUILD, "run_step", side_effect=synthetic_step):
            code = BUILD.main((["--offline"] if offline else []) + ["--bundles", "app"])
        reports = list(self.directory.glob("artifacts/build-0.6.0/*/build-report.json"))
        self.assertEqual(len(reports), 1)
        return code, json.loads(reports[0].read_text()), calls

    def test_failed_full_tests_prevent_bundle_and_app_verification(self):
        code, report, calls = self.run_synthetic_pipeline("full-tests")
        self.assertEqual(code, 1)
        self.assertFalse(report["full_application_build_passed"])
        self.assertEqual(report["failed_step"], "full-tests")
        self.assertNotIn("release-bundle", calls)
        self.assertNotIn("app-verification", calls)

    def test_failed_download_does_not_continue_to_compile(self):
        code, report, calls = self.run_synthetic_pipeline("dependencies", offline=False)
        self.assertEqual(code, 1)
        self.assertFalse(report["full_application_build_passed"])
        self.assertNotIn("full-tests", calls)

    def test_cli_version_mismatch_is_not_silently_accepted(self):
        code, report, calls = self.run_synthetic_pipeline(cli_version="2.9.0")
        self.assertEqual(code, 1)
        self.assertEqual(report["failed_step"], "cli-version-match")
        self.assertEqual(calls, ["cli-version"])

    def test_missing_archive_cannot_become_success_even_if_mocked_commands_pass(self):
        code, report, _ = self.run_synthetic_pipeline()
        self.assertEqual(code, 1)
        self.assertFalse(report["full_application_build_passed"])
        self.assertEqual(report["failed_step"], "app-archive-content")


if __name__ == "__main__":
    unittest.main()
