"""Exercise release manifests and the real installer using local fake downloads."""
import hashlib
import importlib.util
import os
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location("manifest", ROOT / ".github/scripts/generate_release_manifest.py")
manifest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manifest)


class ReleaseInstallTests(unittest.TestCase):
    def test_manifest_contains_final_bytes_and_rejects_unexpected_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            asset = directory / "codypendent.dmg"
            asset.write_bytes(b"final signed bytes")
            output = manifest.generate(directory)
            self.assertEqual(output.read_text(), f"{hashlib.sha256(asset.read_bytes()).hexdigest()}  codypendent.dmg\n")
            (directory / "unexpected.txt").write_text("not a release")
            with self.assertRaisesRegex(ValueError, "unexpected release asset"):
                manifest.generate(directory)

    def test_installer_rejects_changed_download_before_replacing_existing_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = root / "release"
            fixture.mkdir()
            target = "aarch64-apple-darwin"
            source = root / f"codypendent-{target}"
            for name in ["codypendent", "codypendent-ui-worker-launcher", "node-runtime/bin/node", "embedding-runtime/llama-server"]:
                path = source / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("#!/bin/sh\nexit 0\n")
                path.chmod(0o755)
            (source / "embedding-runtime/model.gguf").write_bytes(b"fixture model")
            (source / "embedding-runtime/embedding-runtime.json").write_text("{}")
            archive = fixture / f"codypendent-{target}.tar.gz"
            with tarfile.open(archive, "w:gz") as output:
                output.add(source, arcname=source.name)
            manifest.generate(fixture)
            fake_bin = root / "tools"
            fake_bin.mkdir()
            (fake_bin / "uname").write_text('#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n')
            (fake_bin / "gh").write_text('''#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-D" ]; then shift; destination="$1"; fi
  shift
done
cp "$FIXTURE_DIR"/* "$destination/"
''')
            for script in fake_bin.iterdir(): script.chmod(0o755)
            destination = root / "installed/bin"
            env = dict(os.environ, PATH=f"{fake_bin}:/usr/bin:/bin:/usr/sbin:/sbin",
                       CODYPENDENT_BIN=str(destination), CODYPENDENT_LIB=str(root / "installed/lib/codypendent"),
                       FIXTURE_DIR=str(fixture))
            initial = subprocess.run(["bash", str(ROOT / "install.sh"), "v-test"], check=False, env=env, capture_output=True, text=True)
            self.assertEqual(initial.returncode, 0, initial.stderr)
            self.assertEqual((root / "installed/lib/codypendent/embedding-runtime/model.gguf").read_bytes(), b"fixture model")
            installed = destination / "codypendent"
            installed.write_text("keep the previous installation")
            with archive.open("ab") as output: output.write(b"corruption")
            update = subprocess.run(["bash", str(ROOT / "install.sh"), "v-test"], check=False, env=env, capture_output=True, text=True)
            self.assertNotEqual(update.returncode, 0)
            self.assertIn("checksum mismatch", update.stderr)
            self.assertEqual(installed.read_text(), "keep the previous installation")


if __name__ == "__main__":
    unittest.main()
