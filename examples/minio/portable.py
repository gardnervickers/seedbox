#!/usr/bin/env python3
"""Run pinned-client and evidence controls with socket creation forbidden."""
import argparse
from contextlib import redirect_stdout
import io
from pathlib import Path
import socket
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile
from stage_inputs import PINS, verify_input

HERE = Path(__file__).resolve().parent

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wheel-directory', required=True, type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as directory:
        for pin in PINS['wheels']:
            path = args.wheel_directory / pin['filename']
            verify_input(path, pin)
            with zipfile.ZipFile(path) as wheel:
                wheel.extractall(directory)
        sys.path.insert(0, directory)
        with patch.object(socket, 'socket', side_effect=AssertionError('portable controls forbid sockets')):
            suite = unittest.defaultTestLoader.discover(str(HERE), pattern='test_*.py')
            with redirect_stdout(io.StringIO()):
                result = unittest.TextTestRunner(verbosity=2).run(suite)
        return 0 if result.wasSuccessful() else 1

if __name__ == '__main__':
    sys.exit(main())
