"""仅使用合成归档/签名格式，验证发布脚本不会生成摘要冒充签名。"""
import base64
import io
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import tarfile
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'scripts/generate-update-json.sh'
KEY_ID = b'test-id!'


def synthetic_public_key():
    packet = b'Ed' + KEY_ID + bytes(32)
    text = 'untrusted comment: synthetic key\n' + base64.b64encode(packet).decode() + '\n'
    return base64.b64encode(text.encode()).decode()


def synthetic_signature(key_id=KEY_ID):
    packet = b'ED' + key_id + bytes(64)
    text = 'untrusted comment: synthetic signature\n' + base64.b64encode(packet).decode() \
        + '\ntrusted comment: synthetic\n' + base64.b64encode(bytes(64)).decode() + '\n'
    return base64.b64encode(text.encode()).decode()


class UpdateManifestTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.mkdtemp(prefix='autocode-manifest-test-')
        self.root = Path(self.directory)
        (self.root / 'scripts').mkdir()
        (self.root / 'src-tauri').mkdir()
        shutil.copyfile(SCRIPT, self.root / 'scripts/generate-update-json.sh')
        self.artifact = self.root / 'AutoCode.app.tar.gz'
        self.output = self.root / 'latest.json'
        self.output.write_text('existing-manifest\n')
        self.config(public_key='')

    def config(self, public_key='', version='0.6.0'):
        (self.root / 'src-tauri/tauri.conf.json').write_text(json.dumps({
            'version': version, 'plugins': {'updater': {'pubkey': public_key}},
        }))

    def archive(self, version='0.6.0'):
        data = plistlib.dumps({'CFBundleShortVersionString': version})
        with tarfile.open(self.artifact, 'w:gz') as archive:
            member = tarfile.TarInfo('AutoCode.app/Contents/Info.plist')
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))

    def run_script(self, version='0.6.0', notes='合成测试'):
        return subprocess.run(['bash', str(self.root / 'scripts/generate-update-json.sh'),
                               version, notes, str(self.artifact), str(self.output)],
                              capture_output=True, text=True, timeout=10)

    def assert_preserved(self, result, message):
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(message, result.stderr)
        self.assertEqual(self.output.read_text(), 'existing-manifest\n')

    def test_missing_public_key_preserves_existing_manifest(self):
        self.assert_preserved(self.run_script(), '尚未配置签名更新公钥')

    def test_argument_version_must_match_project(self):
        self.assert_preserved(self.run_script(version='0.5.2'), '版本号与 tauri.conf.json 不一致')

    def test_missing_signature_sidecar_preserves_existing_manifest(self):
        self.config(public_key=synthetic_public_key())
        self.archive()
        self.assert_preserved(self.run_script(), '缺少更新产物的 .sig')

    def test_sha256_is_not_accepted_as_signature(self):
        self.config(public_key=synthetic_public_key())
        self.archive()
        Path(str(self.artifact) + '.sig').write_text('a' * 64)
        self.assert_preserved(self.run_script(), '签名不是 Tauri 生成的')

    def test_archive_version_must_match_manifest(self):
        self.config(public_key=synthetic_public_key())
        self.archive(version='0.5.0')
        # 这里只验证格式/版本处理，不声称这些合成字节具有密码学签名效力。
        Path(str(self.artifact) + '.sig').write_text(synthetic_signature())
        self.assert_preserved(self.run_script(), '归档内应用版本与清单版本不一致')

    def test_structural_manifest_uses_archive_and_escapes_release_notes(self):
        self.config(public_key=synthetic_public_key())
        self.archive()
        signature = synthetic_signature()
        Path(str(self.artifact) + '.sig').write_text(signature)
        notes = '包含 "引号"\n和换行的说明'
        result = self.run_script(notes=notes)
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest = json.loads(self.output.read_text())
        self.assertEqual(manifest['notes'], notes)
        self.assertEqual(manifest['platforms']['darwin-aarch64']['signature'], signature)
        self.assertTrue(manifest['platforms']['darwin-aarch64']['url'].endswith('/AutoCode.app.tar.gz'))

    def test_placeholder_public_key_is_not_accepted(self):
        self.config(public_key='synthetic-public-key')
        self.assert_preserved(self.run_script(), '公钥格式无效')

    def test_signature_key_id_must_match_public_key(self):
        self.config(public_key=synthetic_public_key())
        self.archive()
        Path(str(self.artifact) + '.sig').write_text(synthetic_signature(key_id=b'other-id'))
        self.assert_preserved(self.run_script(), 'key ID 不匹配')


if __name__ == '__main__':
    unittest.main(verbosity=2)
