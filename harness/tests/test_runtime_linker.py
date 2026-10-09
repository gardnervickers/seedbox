"""The Hypervisor entitlement belongs to Seedbox, not dependency executables."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


@unittest.skipUnless(sys.platform == "darwin", "requires Apple's linker and codesign")
class RuntimeLinkerTests(unittest.TestCase):
    def test_only_explicitly_selected_executables_receive_hvf_entitlement(self):
        linker = Path(__file__).resolve().parents[2] / "tools/runtime/linker.py"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "main.c"
            source.write_text("int main(void) { return 0; }\n")
            for selected in (False, True):
                output = root / ("seedbox" if selected else "dependency")
                command = [sys.executable, str(linker), str(source), "-o", str(output)]
                if selected:
                    command.append("--seedbox-sign-hvf")
                subprocess.run(command, check=True, capture_output=True)
                result = subprocess.run(
                    ["/usr/bin/codesign", "--display", "--entitlements", ":-", str(output)],
                    check=True, capture_output=True,
                )
                self.assertEqual(
                    b"com.apple.security.hypervisor" in result.stdout + result.stderr,
                    selected,
                )
