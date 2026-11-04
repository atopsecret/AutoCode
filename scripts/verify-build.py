#!/usr/bin/env python3
"""静态验证 AutoCode 源码与 release App；不启动 App、不调用权限或输入接口。"""
from __future__ import annotations

import argparse
import base64
import datetime
import hashlib
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import subprocess
import sys
from urllib.parse import unquote, urlsplit
from xml.parsers.expat import ExpatError

try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None


def valid_semver(value):
    if not isinstance(value, str):
        return False
    match = re.fullmatch(
        r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
        r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
        r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?", value,
    )
    return bool(match) and all(
        not (part.isdigit() and len(part) > 1 and part.startswith("0"))
        for part in (match.group(4) or "").split(".")
    )


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def within(path, directory):
    return path.resolve().is_relative_to(directory.resolve())


def json_safe(value):
    if isinstance(value, bytes):
        return {"base64": base64.b64encode(value).decode("ascii")}
    if isinstance(value, datetime.datetime):
        return value.isoformat()
    if isinstance(value, dict):
        return {str(key): json_safe(item) for key, item in value.items()}
    if isinstance(value, (tuple, list)):
        return [json_safe(item) for item in value]
    return value


class Resources(HTMLParser):
    def __init__(self):
        super().__init__()
        self.references = []

    def handle_starttag(self, tag, attributes):
        attributes = dict(attributes)
        if tag == "script" and "src" in attributes:
            self.references.append(attributes["src"])
        if tag == "link" and "stylesheet" in attributes.get("rel", "").split():
            self.references.append(attributes.get("href", ""))


def local_resource(reference, parent, frontend):
    if not isinstance(reference, str):
        raise ValueError("资源引用必须是字符串")
    parsed = urlsplit(reference)
    if parsed.scheme or parsed.netloc or not parsed.path:
        raise ValueError("JS/CSS 资源必须是本地文件路径")
    path = (frontend / unquote(parsed.path.lstrip("/")) if parsed.path.startswith("/")
            else parent / unquote(parsed.path)).resolve()
    if not within(path, frontend):
        raise ValueError("资源路径越过 frontendDist")
    return path


class Verification:
    def __init__(self, root, source_only, runner=None):
        self.root = Path(root).resolve()
        self.runner = runner or subprocess.run
        self.report = {
            "schema": 1,
            "scope": "source-only" if source_only else "static-release-bundle",
            "source_root": str(self.root),
            "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "app_executed": False,
            "personal_data_accessed": False,
            "source_only_is_build_success": False,
            "checks": [],
        }

    def check(self, name, passed, detail, **data):
        self.report["checks"].append({"name": name, "passed": bool(passed), "detail": detail, **data})
        return bool(passed)

    def document(self, relative):
        path = self.root / relative
        try:
            raw = path.read_bytes()
            if path.suffix == ".json":
                value = json.loads(raw)
            elif tomllib is None:
                raise ValueError("TOML 校验需要 Python 3.11 或更新版本")
            else:
                value = tomllib.loads(raw.decode("utf-8"))
            if not isinstance(value, dict):
                raise ValueError("配置顶层必须是对象")
            self.check("read:" + relative, True, "读取成功", sha256=hashlib.sha256(raw).hexdigest())
            return value
        except (OSError, ValueError) as error:
            self.check("read:" + relative, False, str(error))
            return None

    def source(self):
        paths = ["package.json", "package-lock.json", "src-tauri/Cargo.toml",
                 "src-tauri/Cargo.lock", "src-tauri/tauri.conf.json"]
        values = [self.document(path) for path in paths]
        if any(value is None for value in values):
            return None
        package, npm_lock, cargo, cargo_lock, config = values
        try:
            locked = [item for item in cargo_lock["package"] if item.get("name") == "autocode"]
            if len(locked) != 1:
                raise ValueError("Cargo.lock 必须恰有一个 autocode 包")
            versions = {
                "package.json": package["version"],
                "package-lock.json": npm_lock["version"],
                "package-lock.json:packages[\"\"]": npm_lock["packages"][""]["version"],
                "Cargo.toml": cargo["package"]["version"],
                "Cargo.lock:autocode": locked[0]["version"],
                "tauri.conf.json": config["version"],
            }
            version = versions["package.json"]
            valid = all(valid_semver(value) for value in versions.values())
            same = valid and len(set(versions.values())) == 1
            self.check("source_versions", same, "六个版本合法且一致" if same else "六个版本必须是同一合法 SemVer", versions=versions)
            self.report["version"] = version
            names = [package["name"], npm_lock["name"], npm_lock["packages"][""]["name"], cargo["package"]["name"]]
            self.check("package_names", all(name == "autocode" for name in names), "npm 与 Cargo 根包应是 autocode", names=names)
            config_dir = self.root / "src-tauri"
            frontend = (config_dir / config["build"]["frontendDist"]).resolve()
            if not within(frontend, self.root):
                raise ValueError("frontendDist 必须位于源码目录内")
            windows = config.get("app", {}).get("windows", [])
            main = next((item.get("url", "index.html") for item in windows if item.get("label") == "main"), "index.html")
            self.resources(frontend, [main, "popup.html"])
            icons = config["bundle"]["icon"]
            if not isinstance(icons, list) or not icons:
                raise ValueError("bundle.icon 必须是非空列表")
            for icon in icons:
                path = (config_dir / icon).resolve()
                self.check("icon:" + icon, within(path, self.root) and path.is_file(), "配置图标必须存在且位于源码目录内")
            mac = config["bundle"].get("macOS", {})
            for key in ("entitlements", "infoPlist"):
                if key in mac:
                    path = (config_dir / mac[key]).resolve()
                    exists = self.check("macOS:" + key, within(path, self.root) and path.is_file(), "配置 plist 必须存在且位于源码目录内")
                    if exists:
                        value = plistlib.loads(path.read_bytes())
                        if not isinstance(value, dict):
                            raise ValueError("配置 plist 必须是字典")
                        if key == "infoPlist":
                            self.check("source:LSUIElement", value.get("LSUIElement") is True,
                                       "源码 Info.plist 必须声明辅助应用 LSUIElement=true")
            return {"config": config, "cargo": cargo, "version": version, "mac": mac}
        except (OSError, KeyError, TypeError, ValueError, AttributeError, StopIteration, ExpatError, plistlib.InvalidFileException) as error:
            self.check("source_structure", False, str(error))
            return None

    def resources(self, frontend, entries):
        queue, seen = [], set()
        for reference in entries:
            try:
                page = local_resource(reference, frontend, frontend)
                if not page.is_file():
                    raise ValueError("入口 HTML 不存在")
                parser = Resources()
                parser.feed(page.read_text(encoding="utf-8"))
                self.check("html:" + reference, True, "入口 HTML 存在")
                queue.extend((ref, page.parent) for ref in parser.references)
            except (OSError, ValueError) as error:
                self.check("html:" + str(reference), False, str(error))
        while queue:
            reference, parent = queue.pop()
            try:
                path = local_resource(reference, parent, frontend)
                if path in seen:
                    continue
                seen.add(path)
                if not path.is_file():
                    raise ValueError("引用的本地资源不存在")
                label = str(path.relative_to(frontend))
                self.check("resource:" + label, True, "本地资源存在")
                if path.suffix in (".js", ".mjs"):
                    text = path.read_text(encoding="utf-8")
                    # 检查静态 ES module 导入，不运行 JS。
                    imports = re.findall(r"(?:import|export)\s+(?:[^;]*?\s+from\s+)?['\"]([^'\"]+)['\"]", text)
                    queue.extend((ref, path.parent) for ref in imports)
            except (OSError, ValueError) as error:
                self.check("resource:" + str(reference), False, str(error))

    def command(self, name, arguments):
        try:
            result = self.runner(arguments, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30, check=False)
            stdout = result.stdout or b""
            stderr = result.stderr or b""
            if isinstance(stdout, str):
                stdout = stdout.encode()
            if isinstance(stderr, str):
                stderr = stderr.encode()
            self.check(name, result.returncode == 0, "命令成功" if result.returncode == 0 else "命令失败", command=arguments,
                       returncode=result.returncode, stdout=stdout.decode("utf-8", "replace")[:8000], stderr=stderr.decode("utf-8", "replace")[:8000])
            return stdout, stderr, result.returncode == 0
        except (OSError, subprocess.TimeoutExpired) as error:
            self.check(name, False, str(error), command=arguments)
            return b"", b"", False

    def app(self, requested, context):
        app = Path(requested).expanduser().resolve()
        self.report["app_path"] = str(app)
        if context is None:
            self.check("app_source_context", False, "源码元信息不完整，不能校验构建产物")
            return
        target = Path(os.environ.get("CARGO_TARGET_DIR", self.root / "src-tauri/target"))
        if not target.is_absolute():
            target = self.root / target
        inside = within(app, target)
        relative = app.relative_to(target.resolve()).parts if inside else ()
        release = any(relative[index:index + 3] == ("release", "bundle", "macos") for index in range(len(relative)))
        expected_name = context["config"].get("productName", "AutoCode") + ".app"
        if not self.check("app_build_location", inside and release and app.name == expected_name,
                          "必须显式指定当前源码 release/bundle/macos 构建产物；不会回退读取已安装 App"):
            return
        if not self.check("app_exists", app.is_dir(), "构建 App 必须存在"):
            return
        if not self.check("macOS_host", platform.system() == "Darwin", "App 签名与 Mach-O 验证需要 macOS"):
            return
        try:
            info_path = app / "Contents/Info.plist"
            if not within(info_path, app):
                raise ValueError("Info.plist 不能指向 bundle 外部")
            info = plistlib.loads(info_path.read_bytes())
            if not isinstance(info, dict):
                raise ValueError("Info.plist 必须是字典")
            config, cargo, mac, version = (context[key] for key in ("config", "cargo", "mac", "version"))
            self.report["bundle_info"] = json_safe(info)
            expected_executable = config.get("mainBinaryName") or cargo["package"]["name"]
            binaries = cargo.get("bin", [])
            if not config.get("mainBinaryName") and len(binaries) == 1:
                expected_executable = binaries[0]["name"]
            expected = {
                "CFBundleShortVersionString": version,
                "CFBundleVersion": mac.get("bundleVersion", version),
                "CFBundleIdentifier": config["identifier"],
                "CFBundleExecutable": expected_executable,
                "LSMinimumSystemVersion": mac.get("minimumSystemVersion", "10.13"),
            }
            for key, value in expected.items():
                self.check("bundle:" + key, info.get(key) == value, "bundle 字段必须与源码配置一致", expected=value, actual=info.get(key))
            self.check("bundle:LSUIElement", info.get("LSUIElement") is True,
                       "AutoCode 辅助应用必须设置 LSUIElement=true", expected=True, actual=info.get("LSUIElement"))
            icon = info.get("CFBundleIconFile")
            icon_path = None
            if isinstance(icon, str) and icon:
                icon_name = icon if Path(icon).suffix else icon + ".icns"
                icon_path = app / "Contents/Resources" / icon_name
            self.check("bundle_icon", icon_path is not None and within(icon_path, app / "Contents/Resources") and icon_path.is_file(),
                       "CFBundleIconFile 对应的 Resources 图标必须存在且不能越过目录", actual=icon,
                       path=str(icon_path) if icon_path is not None else None)
            executable = app / "Contents/MacOS" / expected_executable
            if not self.check("bundle_executable", within(executable, app) and executable.is_file() and os.access(executable, os.X_OK),
                              "bundle 内预期可执行文件必须存在且可执行"):
                return
            self.report["executable_path"] = str(executable)
            self.report["executable_sha256"] = sha256(executable)
            output, _, success = self.command("file", ["/usr/bin/file", "-b", str(executable)])
            self.check("mach_o", success and b"Mach-O" in output and b"executable" in output, "可执行文件必须是 Mach-O executable")
            output, _, success = self.command("lipo", ["/usr/bin/lipo", "-archs", str(executable)])
            architectures = output.decode("ascii", "replace").split()
            host = {"aarch64": "arm64", "AMD64": "x86_64"}.get(platform.machine(), platform.machine())
            self.check("native_architecture", success and host in architectures, "产物必须包含本机架构", host=host, architectures=architectures)
            # 有效签名不保证 dyld 可加载，例如被裁剪破坏的 LINKEDIT 字符串池。
            self.command("dyld_validate", ["/usr/bin/xcrun", "dyld_info", "-validate_only", str(executable)])
            self.command("codesign_verify", ["/usr/bin/codesign", "--verify", "--deep", "--strict", "--verbose=2", str(app)])
            stdout, stderr, success = self.command("codesign_display", ["/usr/bin/codesign", "--display", "--verbose=4", str(app)])
            signature = {}
            for line in (stdout + b"\n" + stderr).decode("utf-8", "replace").splitlines():
                if "=" in line:
                    key, value = line.split("=", 1)
                    signature.setdefault(key, []).append(value)
            self.report["signature"] = signature
            self.check("signature_identifier", success and signature.get("Identifier") == [config["identifier"]],
                       "签名标识必须与配置一致")
            stdout, stderr, success = self.command("codesign_entitlements", ["/usr/bin/codesign", "--display", "--entitlements", ":-", str(app)])
            combined = stdout + b"\n" + stderr
            start, end = combined.find(b"<?xml"), combined.find(b"</plist>")
            if start < 0:
                start = combined.find(b"<plist")
            entitlements = plistlib.loads(combined[start:end + len(b"</plist>")]) if start >= 0 and end >= start else plistlib.loads(stdout)
            if not isinstance(entitlements, dict):
                raise ValueError("签名 entitlement 必须是字典")
            self.report["entitlements"] = json_safe(entitlements)
            configured = mac.get("entitlements")
            expected_entitlements = plistlib.loads((self.root / "src-tauri" / configured).read_bytes()) if configured else {}
            matches = all(entitlements.get(key) == value for key, value in expected_entitlements.items())
            self.check("entitlements_match", success and matches, "配置要求的 entitlement 必须保留在实际签名中")
        except (OSError, ValueError, TypeError, KeyError, AttributeError, ExpatError, plistlib.InvalidFileException) as error:
            self.check("bundle_structure", False, str(error))

    def finish(self):
        self.report["passed"] = bool(self.report["checks"]) and all(item["passed"] for item in self.report["checks"])
        return self.report


def verify(root, *, source_only=False, app=None, runner=None):
    verification = Verification(root, source_only, runner)
    context = verification.source()
    if not source_only:
        if app is None:
            verification.check("app_requested", False, "未指定构建 App，不能报告产物通过")
        else:
            verification.app(app, context)
    return verification.finish()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--source-only", action="store_true", help="只验证源码；不代表 App 构建成功")
    mode.add_argument("--app", type=Path, help="显式指定当前源码 release/bundle/macos 中的构建产物")
    parser.add_argument("--report", type=Path, help="将同一 JSON 验证报告保存到指定文件")
    arguments = parser.parse_args(argv)
    report = verify(arguments.root, source_only=arguments.source_only, app=arguments.app)
    rendered = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if arguments.report:
        try:
            arguments.report.parent.mkdir(parents=True, exist_ok=True)
            arguments.report.write_text(rendered, encoding="utf-8")
        except OSError as error:
            report["checks"].append({"name": "write_report", "passed": False, "detail": str(error)})
            report["passed"] = False
            rendered = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    print(rendered, end="")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
