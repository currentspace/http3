#!/usr/bin/env python3
"""No network, authentication, or publication: exercise the real PTY transport."""
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('bootstrap', Path(__file__).with_name('bootstrap-npm-github.py'))
bootstrap = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bootstrap)
LOGIN = 'https://www.npmjs.com/login?next=/login/cli/11111111-2222-3333-4444-555555555555'
PUBLISH = 'https://www.npmjs.com/auth/cli/11111111-2222-3333-4444-555555555555'


class BootstrapTests(unittest.TestCase):
    def test_challenge_allowlist_excludes_token_polling_and_untrusted_origins(self):
        self.assertEqual(bootstrap.approved_challenge_url(LOGIN, 'login'), LOGIN)
        self.assertEqual(bootstrap.approved_challenge_url(PUBLISH, 'publish'), PUBLISH)
        for url, phase in [(LOGIN + '&token=bad', 'login'), (LOGIN.replace('www.npmjs.com', 'evil.test'), 'login'),
                           (PUBLISH + '#fragment', 'publish'), (PUBLISH + '?token=bad', 'publish'),
                           ('https://registry.npmjs.org/-/v1/login/done/token', 'publish'),
                           (LOGIN, 'publish'), (PUBLISH, 'login')]:
            with self.assertRaises((RuntimeError, ValueError)):
                bootstrap.approved_challenge_url(url, phase)

    def test_real_pty_has_both_ttys_and_only_exports_chunked_browser_url(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)
            program = ('import os,sys,time\n'
                       'assert sys.stdin.isatty() and sys.stdout.isatty()\n'
                       'print("opaque-output-not-for-artifact",flush=True)\n'
                       f'sys.stdout.write({LOGIN[:45]!r});sys.stdout.flush();time.sleep(.05)\n'
                       f'print({LOGIN[45:]!r},flush=True)\n'
                       'print("https://registry.npmjs.org/-/v1/login/done/secret-not-for-artifact",flush=True)\n')
            bootstrap.run_pty([sys.executable, '-c', program], 'login', path, os.environ.copy(), timeout=2)
            receipt = json.loads((path / 'login-challenge.json').read_text())
            self.assertEqual(receipt['url'], LOGIN)
            self.assertEqual(list(path.iterdir()), [path / 'login-challenge.json'])
            self.assertNotIn('secret', (path / 'login-challenge.json').read_text())
            self.assertNotIn('opaque', (path / 'login-challenge.json').read_text())

    def test_pty_error_is_not_success_or_raw_output(self):
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaisesRegex(RuntimeError, 'publish npm command exited 7; raw output withheld') as error:
                bootstrap.run_pty([sys.executable, '-c', 'import sys; print("private-token-value"); sys.exit(7)'],
                                  'publish', Path(temp), os.environ.copy(), timeout=2)
            self.assertNotIn('private-token-value', str(error.exception))

    def test_timeout_terminates_real_child_process(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)
            program = f'import os,time; open({str(path / "pid")!r}, "w").write(str(os.getpid())); time.sleep(30)'
            with self.assertRaisesRegex(RuntimeError, 'timed out'):
                bootstrap.run_pty([sys.executable, '-c', program], 'login', path, os.environ.copy(), timeout=.3)
            pid = int((path / 'pid').read_text())
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)

    def test_wrong_npm_identity_never_reaches_publish_and_erases_credentials(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)
            (path / 'private').mkdir()
            (path / 'private/npmrc').write_text('')
            with patch.object(bootstrap, 'run_pty') as run, patch.object(bootstrap, 'checked_command', return_value=b'wrong-user\n'):
                bootstrap.controller(path)
            self.assertEqual(run.call_count, 1)
            self.assertEqual(json.loads((path / 'status.json').read_text())['phase'], 'failed')
            self.assertFalse((path / 'private').exists())

    def test_environment_never_exports_local_credentials_or_npm_overrides(self):
        with patch.dict(os.environ, {'NPM_TOKEN': 'secret', 'NODE_AUTH_TOKEN': 'secret', 'GH_TOKEN': 'secret',
                                     'npm_config_otp': 'secret', 'NPM_CONFIG_USERCONFIG': '/untrusted',
                                     'RUNNER_TRACKING_ID': 'keep-job-cleanup'}, clear=False):
            env = bootstrap.npm_environment(Path('/tmp/bootstrap-test'))
        for key in ['NPM_TOKEN', 'NODE_AUTH_TOKEN', 'GH_TOKEN', 'npm_config_otp']:
            self.assertNotIn(key, env)
        self.assertEqual(env['RUNNER_TRACKING_ID'], 'keep-job-cleanup')
        self.assertEqual(env['NPM_CONFIG_USERCONFIG'], '/tmp/bootstrap-test/private/npmrc')
        self.assertEqual(env['NPM_CONFIG_IGNORE_SCRIPTS'], 'true')

    def test_cleanup_preserves_challenge_until_artifact_upload(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)
            (path / 'private').mkdir()
            (path / 'private/npmrc').write_text('')
            bootstrap.capture_challenge(PUBLISH + '\n', 'publish', path)
            bootstrap.erase_auth(path)
            self.assertFalse((path / 'private').exists())
            self.assertTrue((path / 'publish-challenge.json').exists())

    def test_cleanup_failures_are_reported_and_not_silent_success(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)
            (path / 'private').mkdir()
            (path / 'private/npmrc').write_text('//registry.npmjs.org/:_authToken=fake-synthetic-token')
            with patch.object(bootstrap.subprocess, 'run') as command:
                command.return_value.returncode = 1
                with self.assertRaisesRegex(RuntimeError, 'revocation failed'):
                    bootstrap.erase_auth(path)
            receipt = json.loads((path / 'public/credential-cleanup.json').read_text())
            self.assertEqual(receipt, {'npm_logout': 'revocation-failed', 'local_credentials_removed': True})
            self.assertFalse((path / 'private').exists())
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)
            (path / 'private').mkdir()
            (path / 'private/npmrc').write_text('')
            with patch.object(bootstrap.shutil, 'rmtree', side_effect=PermissionError()):
                with self.assertRaisesRegex(RuntimeError, 'could not be removed'):
                    bootstrap.erase_auth(path)
            self.assertFalse(json.loads((path / 'public/credential-cleanup.json').read_text())['local_credentials_removed'])


if __name__ == '__main__':
    unittest.main()
