#!/usr/bin/env python3
# Tauri 2 | macOS | Python 3.11+
# Build: python3 scripts/build-local.py
# Run: python3 scripts/verify-build.py --app src-tauri/target/release/bundle/macos/AutoCode.app
# Deps: Rust, Node.js, macOS Command Line Tools；本地或项目内 Tauri CLI
"""锁定依赖构建并静态验证，不安装、不发布、不启动 App。"""

import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import zipfile


ROOT = Path(__file__).resolve().parents[1]


def terminate_group(process):
    """只清理本次创建的进程组，避免编译或下载超时后继续后台运行。"""
    for sig, wait in ((signal.SIGTERM, 3), (signal.SIGKILL, 3)):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            break
        try:
            process.wait(timeout=wait)
            # 父进程可能已结束，仍要清理未结束的同组子进程。
        except subprocess.TimeoutExpired:
            continue
    process.wait(timeout=3)


def run_step(name, command, timeout, report_dir, env, cwd=ROOT):
    log = report_dir / f"{name}.log"
    result = {
        "name": name,
        "command": command,
        "timeout_seconds": timeout,
        "log": str(log),
        "status": "running",
    }
    started = time.monotonic()
    print(f"\n[{name}] {shlex.join(command)}", flush=True)
    with log.open("wb") as output:
        try:
            process = subprocess.Popen(
                command,
                cwd=cwd,
                env=env,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        except OSError as error:
            result.update(status="failed", error=str(error), exit_code=127)
        else:
            try:
                code = process.wait(timeout=timeout)
                result.update(status="passed" if code == 0 else "failed", exit_code=code)
            except subprocess.TimeoutExpired:
                result.update(status="timeout", exit_code=124)
            except KeyboardInterrupt:
                result.update(status="cancelled", exit_code=130)
            finally:
                # 普通失败也可能遗留子进程；成功退出同样不能留下后台任务。
                terminate_group(process)
    result["elapsed_seconds"] = round(time.monotonic() - started, 3)
    print(f"[{name}] {result['status']} ({result['elapsed_seconds']}s)", flush=True)
    if result["status"] != "passed":
        lines = log.read_text(errors="replace").splitlines()
        print("\n".join(lines[-35:]), flush=True)
        if "error" in result:
            print(result["error"], file=sys.stderr)
    return result


def write_report(directory, report):
    destination = directory / "build-report.json"
    temporary = destination.with_suffix(".tmp")
    temporary.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    temporary.replace(destination)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_steps(cli, offline, bundles, dependency_timeout, compile_timeout, verification_report):
    locked = ["--locked"] + (["--offline"] if offline else [])
    steps = [
        ("source", [sys.executable, "scripts/verify-build.py", "--source-only"], 60),
        ("regressions", ["bash", "scripts/test-local.sh"], compile_timeout),
        ("full-tests", ["cargo", "test", *locked, "--manifest-path", "src-tauri/Cargo.toml", "--lib"], compile_timeout),
        ("full-target-check", ["cargo", "check", *locked, "--manifest-path", "src-tauri/Cargo.toml", "--all-targets"], compile_timeout),
        ("release-bundle", [*cli, "build", "--ci", "--bundles", bundles, "--", *locked], compile_timeout),
        ("app-verification", [sys.executable, "scripts/verify-build.py", "--app", "src-tauri/target/release/bundle/macos/AutoCode.app", "--report", str(verification_report)], 60),
    ]
    if not offline:
        # 只取本机目标依赖，不为本次 macOS 构建下载其他平台的依赖。
        machine = platform.machine()
        target = {"arm64": "aarch64-apple-darwin", "x86_64": "x86_64-apple-darwin"}[machine]
        steps.insert(2, ("dependencies", ["cargo", "fetch", "--locked", "--manifest-path", "src-tauri/Cargo.toml", "--target", target], dependency_timeout))
    return steps


def run_pipeline(args, version, report_dir, report, execute):
    # 静态前端无编译依赖。优先复用版本匹配的本地 CLI，不全局安装。
    cli = ["cargo", "tauri"] if shutil.which("cargo-tauri") else ["npm", "run", "tauri", "--"]
    if cli[0] == "npm":
        install = ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"]
        if args.offline:
            install.append("--offline")
        if not execute("project-dependencies", install, args.dependency_timeout):
            return 1
    if not execute("cli-version", [*cli, "--version"], 30):
        return 1
    expected = json.loads((ROOT / "package-lock.json").read_text())["packages"]["node_modules/@tauri-apps/cli"]["version"]
    actual = (report_dir / "cli-version.log").read_text().splitlines()
    if f"tauri-cli {expected}" not in [line.strip() for line in actual]:
        report.update(status="failed", failed_step="cli-version-match", error=f"Tauri CLI 必须匹配 package-lock.json 的 {expected}")
        write_report(report_dir, report)
        print(report["error"], file=sys.stderr)
        return 1

    for name, command, timeout in build_steps(cli, args.offline, args.bundles, args.dependency_timeout, args.compile_timeout, report_dir / "app-verification.json"):
        if not execute(name, command, timeout):
            print(f"构建未通过；完整报告：{report_dir / 'build-report.json'}", file=sys.stderr)
            return 1

    if "dmg" in report["bundles_requested"]:
        images = list((ROOT / "src-tauri/target/release/bundle/dmg").glob(f"AutoCode_{version}_*.dmg"))
        if len(images) != 1:
            report.update(status="failed", failed_step="dmg-artifact", error="必须且只能有一个本版本 DMG；未把旧产物当成新构建")
            write_report(report_dir, report)
            print(report["error"], file=sys.stderr)
            return 1
        if not execute("dmg-verification", ["hdiutil", "verify", str(images[0])], 60):
            return 1
        report["dmg"] = {"path": str(images[0]), "sha256": sha256(images[0])}
    app = ROOT / "src-tauri/target/release/bundle/macos/AutoCode.app"
    archive = report_dir / f"AutoCode_{version}_{platform.machine()}.app.zip"
    if not execute("app-archive", ["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(app), str(archive)], 60):
        return 1
    # 回读归档校验 CRC 及每个普通文件，不把空 ZIP 或旧文件算作产物。
    try:
        with zipfile.ZipFile(archive) as packed:
            if packed.testzip() is not None:
                raise ValueError("App ZIP CRC 校验失败")
            for path in app.rglob("*"):
                if path.is_file() and not path.is_symlink():
                    member = str(Path(app.name) / path.relative_to(app))
                    if hashlib.sha256(packed.read(member)).hexdigest() != sha256(path):
                        raise ValueError(f"App ZIP 内容不匹配：{member}")
    except (OSError, KeyError, ValueError, zipfile.BadZipFile) as error:
        report.update(status="failed", failed_step="app-archive-content", error=str(error))
        write_report(report_dir, report)
        print(report["error"], file=sys.stderr)
        return 1
    report["app_archive"] = {"path": str(archive), "sha256": sha256(archive), "contents_verified": True}
    report.update(status="passed", full_application_build_passed=True, finished_at=dt.datetime.now(dt.timezone.utc).isoformat())
    write_report(report_dir, report)
    print(f"完整构建及静态验证通过：{report_dir / 'build-report.json'}", flush=True)
    print("未启动 App、未替换已安装版本；本地 ad-hoc 签名不是正式签名更新。")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true", help="不访问网络；依赖缺失立即失败")
    parser.add_argument("--bundles", choices=("app", "app,dmg"), default="app,dmg")
    parser.add_argument("--dependency-timeout", type=int, default=90)
    parser.add_argument("--compile-timeout", type=int, default=1200)
    args = parser.parse_args(argv)
    if args.dependency_timeout <= 0 or args.compile_timeout <= 0:
        parser.error("超时时间必须为正整数")
    if sys.platform != "darwin" or platform.machine() not in ("arm64", "x86_64"):
        parser.error("此构建入口仅支持 macOS arm64/x86_64")
    version = json.loads((ROOT / "package.json").read_text())["version"]
    parent = ROOT / "artifacts" / f"build-{version}"
    parent.mkdir(parents=True, exist_ok=True)
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ-")
    report_dir = Path(tempfile.mkdtemp(prefix=stamp, dir=parent))
    report = {
        "version": version,
        "started_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "platform": platform.platform(),
        "architecture": platform.machine(),
        "offline": args.offline,
        "bundles_requested": args.bundles.split(","),
        "status": "running",
        "full_application_build_passed": False,
        "app_launched": False,
        "installed_app_replaced": False,
        "published": False,
        "steps": [],
    }
    env = os.environ.copy()
    # 仅影响子命令；不写网络、npm、Cargo 或用户配置。
    env.update(
        CARGO_TARGET_DIR=str(ROOT / "src-tauri" / "target"),
        CARGO_HTTP_TIMEOUT="15",
        CARGO_NET_RETRY="0",
        npm_config_fetch_retries="0",
        npm_config_fetch_timeout="15000",
        AUTOCODE_TEST_OFFLINE="1" if args.offline else "0",
    )
    print(f"AutoCode {version} 构建；报告：{report_dir}", flush=True)

    def execute(name, command, timeout):
        result = run_step(name, command, timeout, report_dir, env)
        report["steps"].append(result)
        if result["status"] != "passed":
            report.update(status=result["status"], failed_step=name)
        write_report(report_dir, report)
        return result["status"] == "passed"

    def cancel(_signal, _frame):
        raise KeyboardInterrupt

    previous = signal.signal(signal.SIGTERM, cancel)
    try:
        code = run_pipeline(args, version, report_dir, report, execute)
        return 130 if report["status"] == "cancelled" else code
    except KeyboardInterrupt:
        report.update(status="cancelled", full_application_build_passed=False)
        write_report(report_dir, report)
        return 130
    except (OSError, ValueError, KeyError) as error:
        report.update(status="failed", full_application_build_passed=False, error=str(error))
        write_report(report_dir, report)
        print(str(error), file=sys.stderr)
        return 1
    finally:
        signal.signal(signal.SIGTERM, previous)


if __name__ == "__main__":
    sys.exit(main())
