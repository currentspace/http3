import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('musl_check', Path(__file__).parents[1] / 'check-musl-prebuild.py')
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


class MuslPrebuildTest(unittest.TestCase):
    def run_check(self, machine='AArch64', libc='libc.musl-aarch64.so.1', extra='', versions=''):
        def readelf(args, **kwargs):
            if '--file-header' in args:
                output = f'Type: DYN (Shared object file)\nMachine: {machine}\n'
            elif '--dynamic' in args:
                output = f'(NEEDED) Shared library: [{libc}]\n{extra}'
            else:
                output = versions
            return SimpleNamespace(stdout=output)
        with patch.object(checker.subprocess, 'run', side_effect=readelf):
            checker.check('/tmp/http3.linux-arm64-musl.node')

    def test_accepts_musl(self):
        self.run_check(extra='(NEEDED) Shared library: [libgcc_s.so.1]')

    def test_accepts_alpine_libgcc_frame_registration_version(self):
        self.run_check(versions='File: libgcc_s.so.1\n Name: GLIBC_2.0')

    def test_rejects_wrong_architecture(self):
        with self.assertRaisesRegex(ValueError, 'wrong ELF machine'):
            self.run_check(machine='Advanced Micro Devices X86-64')

    def test_rejects_wrong_libc(self):
        with self.assertRaisesRegex(ValueError, 'unexpected runtime dependencies'):
            self.run_check(libc='libc.so.6')

    def test_rejects_missing_build_dependency(self):
        with self.assertRaisesRegex(ValueError, 'unexpected runtime dependencies'):
            self.run_check(extra='(NEEDED) Shared library: [libclang.so.20]')

    def test_rejects_glibc_symbols(self):
        with self.assertRaisesRegex(ValueError, 'glibc symbol dependency'):
            self.run_check(versions='Name: GLIBC_2.34')


if __name__ == '__main__':
    unittest.main()
