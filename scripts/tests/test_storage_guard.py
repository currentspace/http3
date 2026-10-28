import importlib.util
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "storage-guard.py"
SPEC = importlib.util.spec_from_file_location("storage_guard", SCRIPT)
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)


class StorageGuardTests(unittest.TestCase):
    def test_apfs_effective_capacity_and_reserve(self):
        with patch.object(guard, "sample", return_value=(92, 8)):
            guard.check(["/volume"], 94, reserve=1)
            with self.assertRaises(RuntimeError):
                guard.check(["/volume"], 94, reserve=2)

    def test_refuses_to_start_when_space_is_low(self):
        with patch.object(sys, "argv", [str(SCRIPT), "--", "must-not-run"]), \
             patch.object(guard, "sample", return_value=(95, 5)), \
             patch.object(guard.subprocess, "Popen") as spawn:
            self.assertEqual(guard.main(), 125)
            spawn.assert_not_called()

    def test_terminates_running_command_on_threshold_crossing(self):
        process = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"],
                                   start_new_session=True)
        try:
            with patch.object(sys, "argv", [str(SCRIPT), "--", "mock-build"]), \
                 patch.object(guard.subprocess, "Popen", return_value=process), \
                 patch.object(guard, "check", side_effect=[None, RuntimeError("disk full"), None]):
                self.assertEqual(guard.main(), 125)
            self.assertIsNotNone(process.poll())
        finally:
            if process.poll() is None:
                guard.stop(process)


if __name__ == "__main__":
    unittest.main()
