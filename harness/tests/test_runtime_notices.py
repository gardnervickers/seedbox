"""The helper build retains actual notices from upstream source archives."""
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("runtime_notices", ROOT / "tools/runtime/notices.py")
notices = importlib.util.module_from_spec(spec)
spec.loader.exec_module(notices)


def source_archive(path, name, kind="file"):
    with tarfile.open(path, "w") as archive:
        text = f"Synthetic {name} parser-test notice; no upstream code.\n".encode()
        member = tarfile.TarInfo(f"{name}-fixture/" + ("README" if kind == "missing" else "COPYING"))
        if kind == "symlink":
            member.type = tarfile.SYMTYPE
            member.linkname = "/outside"
            archive.addfile(member)
        else:
            member.size = len(text)
            archive.addfile(member, io.BytesIO(text))


class RuntimeNoticeTests(unittest.TestCase):
    def test_actual_notice_bytes_are_retained(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for component in notices.NOTICES:
                source_archive(root / (component + ".source"), component)
            text = notices.notices(root)
            for component in notices.NOTICES:
                self.assertIn(f"Synthetic {component} parser-test notice".encode(), text)

    def test_missing_or_linked_notice_is_rejected_without_extraction(self):
        for kind in ("missing", "symlink"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                source_archive(root / "musl.source", "musl", kind)
                with self.assertRaisesRegex(ValueError, "missing or invalid actual upstream notice"):
                    notices.notices(root, ("musl",))
                self.assertEqual([p.name for p in root.iterdir()], ["musl.source"])
