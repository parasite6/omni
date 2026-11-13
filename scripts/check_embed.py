"""Model-free regressions for omni-embed. Run: python3 -B scripts/check_embed.py."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("omni_embed", Path(__file__).with_name("omni-embed.py"))
embed = importlib.util.module_from_spec(spec)
spec.loader.exec_module(embed)


class FolderTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="omni-embed-check-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.home = self.root / "home"
        self.home.mkdir()
        constants = patch.multiple(embed, HOME=self.home, FOLDERS=self.root / "config/image-folders",
                                   THUMBS=self.root / "thumbs", MAX_BYTES=4)
        constants.start()
        self.addCleanup(constants.stop)
        environment = patch.dict(os.environ, {"HOME": str(self.home)})
        environment.start()
        self.addCleanup(environment.stop)

    def configure(self, text):
        embed.FOLDERS.parent.mkdir(parents=True, exist_ok=True)
        embed.FOLDERS.write_text(text)

    def test_default_is_recursive_pictures_but_empty_config_means_no_folders(self):
        self.assertEqual(embed.read_folders(), [(self.home / "Pictures", True)])
        self.configure("")
        self.assertEqual(embed.read_folders(), [])

    def test_comments_unicode_spaces_and_recursive_suffix(self):
        self.configure("# comment\n\n~/My Pictures/**\n~/été\n")
        self.assertEqual(embed.read_folders(), [(self.home / "My Pictures", True), (self.home / "été", False)])

    def test_adding_existing_folder_can_widen_but_never_narrow_recursion(self):
        self.configure("")
        folder = self.home / "Pictures"
        folder.mkdir()
        self.assertEqual(embed.add_folder(str(folder), False), "new")
        self.assertEqual(embed.add_folder(str(folder / "."), True), "deeper")
        self.assertEqual(embed.add_folder(str(folder), False), "same")
        self.assertEqual(embed.read_folders(), [(folder, True)])
        self.assertEqual(embed.FOLDERS.read_text(), "~/Pictures/**\n")
        self.assertFalse(embed.FOLDERS.with_suffix(".new").exists())

    def test_missing_drive_remains_in_configuration(self):
        available = self.home / "Pictures"
        available.mkdir()
        missing = self.root / "unmounted photos"
        self.configure(f"{available}/**\n{missing}/**\n")
        self.assertEqual(embed.folders(), [(available, True)])
        self.assertEqual(embed.missing_folders(), [missing])
        self.assertEqual(len(embed.read_folders()), 2)

    def test_candidates_obey_recursion_hidden_files_extensions_and_size_limit(self):
        folder = self.home / "Pictures"
        (folder / "sub").mkdir(parents=True)
        (folder / ".hidden").mkdir()
        for name, data in {"keep.PNG": b"1234", "empty.jpg": b"", "large.jpg": b"12345",
                           "notes.txt": b"x", ".secret.png": b"x", "sub/deep.webp": b"x",
                           ".hidden/private.png": b"x"}.items():
            (folder / name).write_bytes(data)
        (folder / "broken.png").symlink_to(folder / "missing.png")
        self.configure(f"{folder}\n")
        flat = embed.candidates()
        self.assertEqual(set(flat), {str(folder / "keep.PNG")})
        self.assertEqual(flat[str(folder / "keep.PNG")], ((folder / "keep.PNG").stat().st_mtime_ns, 4))
        self.configure(f"{folder}/**\n")
        self.assertEqual(set(embed.candidates()), {str(folder / "keep.PNG"), str(folder / "sub/deep.webp")})

    def test_thumbnail_keys_distinguish_same_basename_in_different_folders(self):
        first = embed.thumb_path("/one/photo.png")
        second = embed.thumb_path("/two/photo.png")
        self.assertNotEqual(first, second)
        self.assertEqual(first, embed.thumb_path("/one/photo.png"))
        self.assertEqual(first.parent, embed.THUMBS)
        self.assertEqual(first.suffix, ".png")


if __name__ == "__main__":
    unittest.main(verbosity=2)
