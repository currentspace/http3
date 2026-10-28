import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ENTRYPOINT = Path(__file__).resolve().parents[2] / "examples/hono/acme-entrypoint.sh"


class AcmeEntrypointTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        (self.bin / "python3").symlink_to(sys.executable)
        self.env = {
            **os.environ, "PATH": f"{self.bin}:/usr/bin:/bin", "ACME_ENABLED": "1",
            "ACME_DOMAIN": "test.invalid", "ACME_LOCK_TABLE": "mock",
            "ACME_CERT_SECRET_ID": "mock", "CF_TOKEN": "mock-test-token",
            "ACME_FAIL_OPEN": "0", "ACME_DIR": str(self.root / "certs"),
            "ACME_BOOTSTRAP_RETRIES": "1", "ACME_RETRY_SLEEP_SEC": "0",
            "TEST_CALLS": str(self.root / "calls"),
        }
        self.command("node", 'printf "node\\n" >> "$TEST_CALLS"')
        self.command("aws", 'printf "unexpected aws\\n" >> "$TEST_CALLS"; exit 99')
        self.command("curl", 'printf "unexpected curl\\n" >> "$TEST_CALLS"; exit 99')

    def command(self, name, body):
        path = self.bin / name
        path.write_text("#!/bin/sh\n" + body + "\n")
        path.chmod(0o755)

    def run_entrypoint(self):
        return subprocess.run(["/bin/bash", str(ENTRYPOINT)], env=self.env,
                              capture_output=True, text=True, timeout=10)

    def test_disabled_starts_app_without_certificate_operations(self):
        self.env["ACME_ENABLED"] = "0"
        result = self.run_entrypoint()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "calls").read_text(), "node\n")

    def test_missing_client_fails_before_network_or_app(self):
        result = self.run_entrypoint()
        self.assertEqual(result.returncode, 1)
        self.assertIn("acme.sh is missing", result.stdout)
        self.assertFalse((self.root / "calls").exists())

    def test_existing_client_and_valid_secret_start_app(self):
        self.command("acme.sh", 'printf "unexpected issuance\\n" >> "$TEST_CALLS"; exit 99')
        secret = json.dumps({"key": "fixture-key", "cert": "fixture-cert"})
        self.command("aws", "printf '%s' '" + secret + "'")
        self.command("openssl", "exit 0")
        self.command("node", 'test -s "$HTTP3_TLS_CERT_PATH" && test -s "$HTTP3_TLS_KEY_PATH" && printf "node\\n" >> "$TEST_CALLS"')
        result = self.run_entrypoint()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "calls").read_text(), "node\n")


if __name__ == "__main__":
    unittest.main()
