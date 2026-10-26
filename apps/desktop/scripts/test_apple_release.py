"""Small packaging regressions; no identities, network or real binaries used."""
import importlib.util
import plistlib
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("apple_release", Path(__file__).with_name("apple-release.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class AppleReleaseTests(unittest.TestCase):
    def setUp(self):
        model = {**release.EMBEDDING_RELEASE["model"], "sha256": release.hashlib.sha256(b"fixture model").hexdigest()}
        self.pins = patch.object(release, "EMBEDDING_RELEASE", {"model": model})
        self.pins.start()
        self.addCleanup(self.pins.stop)

    def embeddings(self, parent):
        root = parent / "embedding-runtime"
        root.mkdir()
        (root / "llama-server").write_bytes(b"fixture server")
        (root / "llama-server").chmod(0o755)
        (root / "model.gguf").write_bytes(b"fixture model")
        model = release.EMBEDDING_RELEASE["model"]
        manifest = {"model": model["name"], "modelSha256": model["sha256"], "dimensions": model["dimensions"], "promptFormat": model["promptFormat"]}
        (root / release.EMBEDDING_MANIFEST).write_text(release.json.dumps(manifest))
        release.reseal_embeddings(root)
        return root

    def test_embedding_model_tampering_cannot_be_resealed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = self.embeddings(Path(tmp))
            release.verify_embeddings(root)
            (root / "model.gguf").write_bytes(b"changed model")
            with self.assertRaisesRegex(ValueError, "pinned checksum"):
                release.reseal_embeddings(root)

    def test_embedding_signature_changes_require_resealing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = self.embeddings(Path(tmp))
            (root / "llama-server").write_bytes(b"signed server")
            with self.assertRaisesRegex(ValueError, "final byte seal"):
                release.verify_embeddings(root)
            release.reseal_embeddings(root)
            release.verify_embeddings(root)

    def test_runtime_directory_name_does_not_prove_hardened_runtime(self):
        detail = ("Executable=/Applications/Codypendent.app/Contents/MacOS/node-runtime/bin/node\n"
                  "CodeDirectory v=20500 size=500 flags=0x0(none) hashes=1+0 location=embedded\n"
                  "Authority=Developer ID Application: Test (TEAM)\nTimestamp=4 Oct 2026\n")
        self.assertFalse(release.distribution_signature(detail))
        self.assertTrue(release.distribution_signature(detail.replace("flags=0x0(none)", "flags=0x10000(runtime)")))
        self.assertFalse(release.distribution_signature(detail.replace("flags=0x0(none)", "flags=0x10000(runtime)").replace("Timestamp=4 Oct 2026\n", "")))

    def test_modified_runtime_is_rejected_until_resealed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            node = root / "bin/node"
            node.parent.mkdir()
            node.write_bytes(b"node before signing")
            release.reseal(root)
            original = (root / release.SEAL).read_text()
            node.write_bytes(b"node after signing")
            self.assertNotEqual(release.json.loads(original), release.runtime_entries(root))
            release.reseal(root)
            self.assertEqual(release.json.loads((root / release.SEAL).read_text()), release.runtime_entries(root))

    def test_symlink_cannot_escape_runtime(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "escape").symlink_to("/usr/bin/env")
            with self.assertRaisesRegex(ValueError, "leaves its root"):
                release.runtime_entries(root)

    def test_cli_distribution_rejects_unsigned_and_unnotarized_binaries(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in ["codypendent", "codypendent-ui-worker-launcher", "codypendentd", "monty", "node-runtime/bin/node"]:
                target = root / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(b"fixture executable")
                target.chmod(0o755)
            release.reseal(root / "node-runtime")
            self.embeddings(root)
            with patch.object(release, "run", side_effect=lambda *args: "arm64" if args[0] == "lipo" else "Signature=adhoc"):
                release.verify_runtime(root, False)
                with self.assertRaisesRegex(ValueError, "Developer ID"):
                    release.verify_runtime(root, True)
            signature = ("CodeDirectory v=20500 flags=0x10000(runtime)\n"
                         "Authority=Developer ID Application: Test (TEAM)\nTimestamp=4 Oct 2026\n")
            def unnotarized(*args):
                if "--check-notarization" in args:
                    raise ValueError("notarization requirement failed")
                return "arm64" if args[0] == "lipo" else signature
            with patch.object(release, "run", side_effect=unnotarized), self.assertRaisesRegex(ValueError, "notarization requirement failed"):
                release.verify_runtime(root, True)

    def test_stage_produces_complete_bundle_and_rejects_overwrite(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            app = root / "Codypendent.app"
            binary = app / "Contents/MacOS/codypendent-desktop"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"desktop")
            binary.chmod(0o755)
            (app / "Contents/Info.plist").write_bytes(plistlib.dumps({
                "CFBundleIdentifier": "com.codypendent.desktop",
                "CFBundleExecutable": "codypendent-desktop",
                "CFBundleShortVersionString": "0.14.0",
            }))
            source = root / "runtime"
            for name in ["codypendent", "codypendent-ui-worker-launcher", "monty", "node-runtime/bin/node"]:
                target = source / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(name.encode())
                target.chmod(0o755)
            header = source / "node-runtime/include/node/v8-callbacks.h"
            header.parent.mkdir(parents=True)
            header.write_bytes(b"fixture Node header")
            release.reseal(source / "node-runtime")
            self.embeddings(source)
            def command(*args):
                return "arm64" if args[0] == "lipo" else "codypendent 0.14.0"
            with patch.object(release, "run", side_effect=command), patch.object(release.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "", "unsigned")):
                release.stage(app, source)
                release.verify(app, False)
                runtime_link = app / "Contents/MacOS/node-runtime"
                self.assertTrue(runtime_link.is_symlink())
                self.assertEqual(runtime_link.resolve(), (app / "Contents/Resources/node-runtime").resolve())
                self.assertEqual((runtime_link / "include/node/v8-callbacks.h").read_bytes(), b"fixture Node header")
                (app / "Contents/MacOS/monty").unlink()
                with self.assertRaisesRegex(ValueError, "missing executable:.*monty"):
                    release.verify(app, False)
                with self.assertRaisesRegex(ValueError, "already contains"):
                    release.stage(app, source)


if __name__ == "__main__":
    unittest.main()
