"""Pinned embedding packaging checks without network access or real weights."""
import hashlib
import importlib.util
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("prepare_embeddings", Path(__file__).with_name("prepare-embedding-runtime.py"))
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class EmbeddingPreparationTests(unittest.TestCase):
    def asset(self, name, data):
        return {"filename": name, "size": len(data), "sha256": hashlib.sha256(data).hexdigest(), "url": "https://example.invalid/" + name}

    def test_bad_download_does_not_replace_cached_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cached = root / "model.gguf"
            cached.write_bytes(b"old model")
            with patch.object(prepare.urllib.request, "urlopen", return_value=io.BytesIO(b"corrupt")), self.assertRaisesRegex(ValueError, "pinned checksum"):
                prepare.download(self.asset("model.gguf", b"new model"), root)
            self.assertEqual(cached.read_bytes(), b"old model")
            self.assertEqual(list(root.glob(".download-*")), [])

    def test_unsupported_target_fails_before_downloading(self):
        with patch.object(prepare, "download") as download, self.assertRaisesRegex(ValueError, "No pinned embedding runtime"):
            prepare.prepare("unsupported-target", Path("unused"))
        download.assert_not_called()

    def archive(self, path, unsafe_link=False):
        with tarfile.open(path, "w:gz") as archive:
            for name, data in [("llama-server", b"server"), ("libllama.dylib", b"library"), ("llama-cli", b"unused CLI")]:
                entry = tarfile.TarInfo("runtime/" + name)
                entry.size = len(data)
                archive.addfile(entry, io.BytesIO(data))
            if unsafe_link:
                entry = tarfile.TarInfo("runtime/libescape.dylib")
                entry.type = tarfile.SYMTYPE
                entry.linkname = "../../outside"
                archive.addfile(entry)

    def test_unpack_keeps_server_libraries_and_excludes_unneeded_executables(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            archive, output = root / "runtime.tar.gz", root / "unpacked"
            output.mkdir()
            self.archive(archive)
            prepare.unpack(archive, output)
            self.assertEqual(sorted(path.name for path in output.iterdir()), ["libllama.dylib", "llama-server"])

    def test_archive_links_cannot_escape_the_library_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            archive, output = root / "runtime.tar.gz", root / "unpacked"
            output.mkdir()
            self.archive(archive, unsafe_link=True)
            with self.assertRaisesRegex(ValueError, "link leaves its directory"):
                prepare.unpack(archive, output)

    def test_preparation_includes_verified_weights_licences_and_manifest_offline(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cache = root / "target/embedding-runtime-cache"
            cache.mkdir(parents=True)
            archive = cache / "runtime.tar.gz"
            self.archive(archive)
            (cache / "weights.gguf").write_bytes(b"weights")
            (cache / "licence.txt").write_bytes(b"licence")
            model = {**prepare.RELEASE["model"], **self.asset("weights.gguf", b"weights")}
            asset = self.asset(archive.name, archive.read_bytes())
            release = {"model": model, "runtime": {"version": "test", "revision": "test", "assets": {"test-target": asset}}, "licenses": [self.asset("licence.txt", b"licence")]}
            with patch.object(prepare, "REPOSITORY", root), patch.object(prepare, "RELEASE", release), patch.object(prepare.urllib.request, "urlopen", side_effect=AssertionError("network used")):
                destination = prepare.prepare("test-target", root / "output", tauri=False)
            manifest = json.loads((destination / prepare.MANIFEST).read_text())
            self.assertEqual((destination / "licenses/licence.txt").read_bytes(), b"licence")
            self.assertEqual(manifest["modelSha256"], model["sha256"])
            self.assertEqual(manifest["serverSha256"], hashlib.sha256(b"server").hexdigest())
            self.assertEqual(len(manifest["files"]), 4)


if __name__ == "__main__":
    unittest.main()
