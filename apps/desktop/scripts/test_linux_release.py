"""Linux staging regressions with synthetic ELF payloads, no network or providers."""
import importlib.util
import json
import os
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("linux_release", Path(__file__).with_name("linux-release.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class LinuxReleaseTests(unittest.TestCase):
    def setUp(self):
        self.config = {"productName": "Codypendent", "version": "0.14.0"}
        model = {**release.apple.EMBEDDING_RELEASE["model"], "sha256": release.apple.hashlib.sha256(b"fixture model").hexdigest()}
        self.pins = patch.object(release.apple, "EMBEDDING_RELEASE", {"model": model})
        self.pins.start()
        self.addCleanup(self.pins.stop)
        self.version = patch.object(release.apple, "run", return_value="codypendent 0.14.0")
        self.version.start()
        self.addCleanup(self.version.stop)

    def executable(self, path):
        path.parent.mkdir(parents=True, exist_ok=True)
        header = bytearray(64)
        header[:6] = b"\x7fELF\x02\x01"
        header[18:20] = (62).to_bytes(2, "little")
        path.write_bytes(bytes(header) + path.name.encode())
        path.chmod(0o755)

    def payload(self, root):
        for name in ["codypendent", "codypendent-ui-worker-launcher", "monty", "node-runtime/bin/node", "embedding-runtime/llama-server"]:
            self.executable(root / name)
        header = root / "node-runtime/include/node/v8.h"
        header.parent.mkdir(parents=True)
        header.write_text("fixture complete Node distribution")
        npm = root / "node-runtime/lib/node_modules/npm/bin/npm-cli.js"
        npm.parent.mkdir(parents=True)
        npm.write_text("fixture npm entry point")
        (root / "node-runtime/bin/npm").symlink_to("../lib/node_modules/npm/bin/npm-cli.js")
        release.apple.reseal(root / "node-runtime")
        embeddings = root / "embedding-runtime"
        (embeddings / "model.gguf").write_bytes(b"fixture model")
        model = release.apple.EMBEDDING_RELEASE["model"]
        (embeddings / release.apple.EMBEDDING_MANIFEST).write_text(json.dumps({
            "model": model["name"], "modelSha256": model["sha256"], "dimensions": model["dimensions"], "promptFormat": model["promptFormat"],
        }))
        release.apple.reseal_embeddings(embeddings)
        return root

    def stage_appdir(self, root):
        source = self.payload(root / "source")
        appdir = root / "Codypendent.AppDir"
        self.executable(appdir / "usr/bin/codypendent-desktop")
        destination = appdir / "usr/share/Codypendent/runtime"
        overlay = root / "linux-runtime.json"
        release.stage(source, destination, overlay, self.config)
        return source, appdir, destination, overlay

    def test_complete_runtime_is_installed_and_verified_without_path(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, {"PATH": ""}):
            source, appdir, destination, overlay = self.stage_appdir(Path(temporary))
            release.verify_appdir(appdir, self.config)
            for name in ["codypendent", "codypendent-ui-worker-launcher", "monty", "node-runtime/bin/node", "node-runtime/include/node/v8.h", "embedding-runtime/model.gguf"]:
                self.assertEqual((destination / name).read_bytes(), (source / name).read_bytes())
            config = json.loads(overlay.read_text())
            files = {"usr/share/Codypendent/runtime": str(destination)}
            self.assertEqual(config, {"bundle": {"linux": {"appimage": {"files": files}, "deb": {"files": files}}}})
            self.assertTrue((destination / "node-runtime/bin/npm").is_symlink())
            self.assertEqual(os.readlink(destination / "node-runtime/bin/npm"), "../lib/node_modules/npm/bin/npm-cli.js")
            self.assertNotIn("resources", config["bundle"], "directory copies must preserve the sealed Node links")
            self.assertNotIn("active", config["bundle"], "release staging cannot disable bundling")

    def test_tauri_overlay_keeps_sealed_payload_outside_linuxdeploy_elf_scan(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = self.payload(root / "source")
            staged, overlay = root / "staged", root / "overlay.json"
            release.stage(source, staged, overlay, self.config)
            appdir = root / "Codypendent.AppDir"
            self.executable(appdir / "usr/bin/codypendent-desktop")
            library = appdir / "usr/lib/libfixture.so"
            self.executable(library)
            files = json.loads(overlay.read_text())["bundle"]["linux"]["appimage"]["files"]
            for destination, source_path in files.items():
                shutil.copytree(source_path, appdir / destination, symlinks=True)
            runtime = appdir / next(iter(files))
            before = release.entries(runtime)
            scanned = [*(appdir / "usr/bin").iterdir(), *(appdir / "usr/lib").rglob("*")]
            rewritten = []
            for path in scanned:
                if path.is_file() and not path.is_symlink() and path.read_bytes().startswith(b"\x7fELF"):
                    with path.open("ab") as handle:
                        handle.write(b"linuxdeploy-rpath-rewrite")
                    rewritten.append(path)
            self.assertIn(library, rewritten)
            self.assertFalse(any(path.is_relative_to(runtime) for path in rewritten))
            self.assertEqual(release.entries(runtime), before)
            self.assertEqual(before, release.entries(staged))
            self.assertTrue((runtime / "node-runtime/bin/npm").is_symlink())
            release.verify_appdir(appdir, self.config)

    def test_legacy_lib_tree_cannot_replace_the_required_share_payload(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, appdir, runtime, _ = self.stage_appdir(Path(temporary))
            legacy = appdir / "usr/lib/Codypendent/runtime"
            legacy.parent.mkdir(parents=True)
            runtime.rename(legacy)
            with self.assertRaises(FileNotFoundError):
                release.verify_appdir(appdir, self.config)

    def test_missing_worker_and_mismatched_version_never_create_staging(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = self.payload(root / "source")
            worker = source / "codypendent-ui-worker-launcher"
            worker.unlink()
            destination, overlay = root / "output", root / "overlay.json"
            with self.assertRaisesRegex(ValueError, "Required Linux executable.*worker"):
                release.stage(source, destination, overlay, self.config)
            self.executable(worker)
            with patch.object(release.apple, "run", return_value="codypendent 0.13.0"), self.assertRaisesRegex(ValueError, "versions differ"):
                release.stage(source, destination, overlay, self.config)
            self.assertFalse(destination.exists())
            self.assertFalse(overlay.exists())

    def test_node_tampering_cannot_be_staged(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = self.payload(root / "source")
            with (source / "node-runtime/bin/node").open("ab") as handle:
                handle.write(b"tampered")
            with self.assertRaisesRegex(ValueError, "Node runtime.*seal"):
                release.stage(source, root / "output", root / "overlay.json", self.config)
            self.assertFalse((root / "output").exists())

    def test_installed_runtime_rejects_missing_or_modified_payload_members(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, appdir, destination, _ = self.stage_appdir(root)
            worker = destination / "codypendent-ui-worker-launcher"
            worker.unlink()
            with self.assertRaisesRegex(ValueError, "Required Linux executable.*worker"):
                release.verify_appdir(appdir, self.config)
            shutil.copy2(source / worker.name, worker)
            with worker.open("ab") as handle:
                handle.write(b"modified worker")
            with self.assertRaisesRegex(ValueError, "staged byte seal"):
                release.verify_appdir(appdir, self.config)

    def test_wrong_architecture_or_escaping_link_is_refused(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = self.payload(root / "source")
            node = source / "node-runtime/bin/node"
            data = bytearray(node.read_bytes())
            data[18:20] = (183).to_bytes(2, "little")
            node.write_bytes(data)
            with self.assertRaisesRegex(ValueError, "x86-64 Linux ELF"):
                release.verify_payload(source, self.config)
            self.executable(node)
            release.apple.reseal(source / "node-runtime")
            (source / "foreign").symlink_to(root / "outside")
            with self.assertRaisesRegex(ValueError, "symlink leaves"):
                release.verify_payload(source, self.config)

    def test_unexpected_existing_staging_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = self.payload(root / "source")
            destination = root / "existing"
            destination.mkdir()
            marker = destination / "keep"
            marker.write_text("keep existing data")
            with self.assertRaisesRegex(ValueError, "already exists"):
                release.stage(source, destination, root / "overlay.json", self.config)
            self.assertEqual(marker.read_text(), "keep existing data")


if __name__ == "__main__":
    unittest.main()
