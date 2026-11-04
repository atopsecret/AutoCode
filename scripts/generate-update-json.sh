#!/bin/bash
set -euo pipefail

# Tauri macOS 更新使用 .app.tar.gz 与真实签名，不使用 DMG 的 SHA-256 冒充签名。
# Build: npm run tauri build（需配置 TAURI_SIGNING_PRIVATE_KEY）
# Run: bash scripts/generate-update-json.sh VERSION NOTES [ARTIFACT] [OUTPUT]
if [ $# -lt 1 ] || [ $# -gt 4 ]; then
  echo "用法: $0 <version> [notes] [AutoCode.app.tar.gz] [latest.json]" >&2
  exit 1
fi

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
VERSION=$1
NOTES=${2:-}
ARTIFACT=${3:-$ROOT/src-tauri/target/release/bundle/macos/AutoCode.app.tar.gz}
OUTPUT=${4:-$ROOT/latest.json}

python3 - "$ROOT" "$VERSION" "$NOTES" "$ARTIFACT" "$OUTPUT" <<'PY'
import base64
import datetime
import json
from pathlib import Path
import re
import sys
import tarfile
import plistlib
import os
import tempfile

root, version, notes, artifact_name, output_name = sys.argv[1:]
root = Path(root)
artifact = Path(artifact_name)
output = Path(output_name)

def fail(message):
    raise SystemExit("错误: " + message)

if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?", version):
    fail("版本号不是有效的 release 版本")
config = json.loads((root / "src-tauri/tauri.conf.json").read_text())
if config["version"] != version:
    fail("版本号与 tauri.conf.json 不一致")
public_key = config.get("plugins", {}).get("updater", {}).get("pubkey", "").strip()
if not public_key:
    fail("尚未配置签名更新公钥；不会生成可安装更新清单")
try:
    key_text = base64.b64decode(public_key, validate=True).decode("utf-8")
    key_lines = key_text.splitlines()
    key_packet = base64.b64decode(key_lines[1], validate=True)
    if len(key_lines) != 2 or not key_lines[0].startswith("untrusted comment:") \
            or len(key_packet) != 42 or key_packet[:2] != b"Ed":
        raise ValueError("invalid key packet")
except (ValueError, UnicodeDecodeError, IndexError):
    fail("签名更新公钥格式无效；不接受占位符")
if artifact.name != "AutoCode.app.tar.gz" or not artifact.is_file():
    fail("缺少 Tauri 生成的 AutoCode.app.tar.gz 更新产物")
signature_path = Path(str(artifact) + ".sig")
if not signature_path.is_file():
    fail("缺少更新产物的 .sig 签名文件；SHA-256 不能替代签名")
signature = signature_path.read_text().strip()
if not signature:
    fail("签名文件为空")
try:
    decoded = base64.b64decode(signature, validate=True).decode("utf-8")
except (ValueError, UnicodeDecodeError):
    fail("签名不是 Tauri 生成的 base64 minisign 签名")
try:
    signature_lines = decoded.splitlines()
    signature_packet = base64.b64decode(signature_lines[1], validate=True)
    global_signature = base64.b64decode(signature_lines[3], validate=True)
    if len(signature_lines) != 4 or not signature_lines[0].startswith("untrusted comment:") \
            or not signature_lines[2].startswith("trusted comment:") \
            or len(signature_packet) != 74 or signature_packet[:2] not in (b"Ed", b"ED") \
            or len(global_signature) != 64:
        raise ValueError("invalid signature packet")
    if signature_packet[2:10] != key_packet[2:10]:
        fail("签名与发布公钥的 key ID 不匹配")
except (ValueError, IndexError):
    fail("签名格式不合法；不接受 SHA-256 摘要")

# 只读取归档中的版本元数据，不解压，不执行产物。
try:
    with tarfile.open(artifact, "r:gz") as archive:
        plist_members = [m for m in archive.getmembers()
                         if m.name.rstrip("/").endswith("AutoCode.app/Contents/Info.plist") and m.isfile()]
        if len(plist_members) != 1 or plist_members[0].size > 1024 * 1024:
            fail("更新归档缺少唯一的应用 Info.plist")
        metadata = plistlib.load(archive.extractfile(plist_members[0]))
        if metadata.get("CFBundleShortVersionString") != version:
            fail("归档内应用版本与清单版本不一致")
except (tarfile.TarError, OSError, plistlib.InvalidFileException) as error:
    fail("无法读取更新归档: " + str(error))

manifest = {
    "version": version,
    "notes": notes,
    "pub_date": datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z"),
    "platforms": {
        "darwin-aarch64": {
            "signature": signature,
            "url": f"https://github.com/skernelx/AutoCode/releases/download/v{version}/AutoCode.app.tar.gz",
        },
    },
}
output.parent.mkdir(parents=True, exist_ok=True)
with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=output.parent,
                                 prefix=".latest-", suffix=".tmp", delete=False) as temporary:
    json.dump(manifest, temporary, ensure_ascii=False, indent=2)
    temporary.write("\n")
    temporary.flush()
    os.fsync(temporary.fileno())
    temporary_name = temporary.name
os.replace(temporary_name, output)
print(f"已生成签名更新清单: {output}")
print("签名真实性由更新客户端使用发布公钥验证；脚本不生成或替代签名。")
PY
