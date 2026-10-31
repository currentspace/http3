#!/usr/bin/env python3
"""One-time, fixed-target npm bootstrap on a GitHub-hosted runner.

Only npm's browser approval URL leaves the PTY. Credentials, token-poll URLs,
and raw CLI output stay on the runner and are never artifact inputs.
"""
import argparse
import base64
from datetime import datetime, timezone
import hashlib
import io
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile

REPOSITORY = 'currentspace/http3'
RUN_ID = 41176836293
SOURCE_SHA = '95109299a5fde5c022d5774d81ce14c00d7923f1'
ARTIFACT_ID = 13033223790
ARTIFACT_NAME = 'prebuild-x86_64-unknown-linux-musl'
ARCHIVE_SHA256 = '4b0b3e9195a88764c519fe7ca41a190d2435c79ac8834d3b7489196fd1832343'
BINARY = 'http3.linux-x64-musl.node'
BINARY_SHA256 = '8b12296c1b2716c97f2f2199292fe28134adf779a5da3c286415f6d8c9b51854'
PACKAGE = '@currentspace/http3-linux-x64-musl'
VERSION = '1.0.0-bootstrap.0'
EXPECTED_USER = 'brianmeek'
REGISTRY = 'https://registry.npmjs.org/'
ROOT = Path(__file__).resolve().parent.parent
UUID = r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}'


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')
    temporary.replace(path)


def read_json(path):
    return json.loads(path.read_text())


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def runner_directory():
    require(os.environ.get('GITHUB_ACTIONS') == 'true', 'This command requires GitHub Actions')
    require(os.environ.get('GITHUB_REPOSITORY') == REPOSITORY, 'Unexpected repository')
    require(os.environ.get('GITHUB_REF') == 'refs/heads/main', 'Bootstrap requires main')
    require(os.environ.get('GITHUB_EVENT_NAME') == 'workflow_dispatch', 'Bootstrap requires manual dispatch')
    require(os.environ.get('RUNNER_ENVIRONMENT') == 'github-hosted', 'Bootstrap requires a GitHub-hosted runner')
    return Path(os.environ['RUNNER_TEMP']) / 'http3-npm-bootstrap'


def npm_environment(directory):
    env = {key: value for key, value in os.environ.items()
           if not key.lower().startswith('npm_config_')
           and key not in {'NPM_TOKEN', 'NODE_AUTH_TOKEN', 'GH_TOKEN', 'GITHUB_TOKEN', 'NODE_OPTIONS'}}
    env.update(NPM_CONFIG_USERCONFIG=str(directory / 'private' / 'npmrc'),
               NPM_CONFIG_GLOBALCONFIG='/dev/null', NPM_CONFIG_CACHE=str(directory / 'private' / 'cache'),
               NPM_CONFIG_LOGS_MAX='0', NPM_CONFIG_BROWSER='false', NPM_CONFIG_REGISTRY=REGISTRY,
               NPM_CONFIG_IGNORE_SCRIPTS='true', NPM_CONFIG_PROVENANCE='true',
               TMPDIR=str(directory / 'private'))
    return env


def checked_command(command, **kwargs):
    result = subprocess.run(command, capture_output=True, timeout=120, **kwargs)
    require(result.returncode == 0, f'{Path(command[0]).name} command failed (exit {result.returncode}); raw output withheld')
    return result.stdout


def github_api(path):
    return checked_command(['gh', 'api', f'repos/{REPOSITORY}/{path}'])


def anonymous_get(url):
    parsed = urllib.parse.urlparse(url)
    require(parsed.scheme == 'https' and parsed.hostname == 'registry.npmjs.org'
            and not parsed.username and not parsed.password, 'Unexpected registry URL')
    request = urllib.request.Request(url, headers={'Accept': 'application/json', 'User-Agent': 'http3-bootstrap-readback'})
    with urllib.request.urlopen(request, timeout=45) as response:
        require(urllib.parse.urlparse(response.geturl()).hostname == 'registry.npmjs.org', 'Unexpected registry redirect')
        data = response.read(100 * 1024 * 1024 + 1)
    require(len(data) <= 100 * 1024 * 1024, 'Registry response too large')
    return data


def version_metadata():
    try:
        return json.loads(anonymous_get(REGISTRY + urllib.parse.quote(PACKAGE, safe='@') + '/' + VERSION))
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def prepare(directory):
    require(not directory.exists(), 'Bootstrap state already exists; use a fresh job')
    os.umask(0o077)
    (directory / 'private').mkdir(parents=True)
    (directory / 'private' / 'npmrc').touch(mode=0o600)
    (directory / 'public').mkdir()
    run = json.loads(github_api(f'actions/runs/{RUN_ID}'))
    require(run['head_sha'] == SOURCE_SHA and run['conclusion'] == 'success'
            and run['status'] == 'completed' and run['path'] == '.github/workflows/release.yml'
            and run['head_branch'] == 'main' and run['event'] == 'workflow_dispatch', 'Qualified run mismatch')
    artifact = json.loads(github_api(f'actions/artifacts/{ARTIFACT_ID}'))
    require(artifact['name'] == ARTIFACT_NAME and not artifact['expired']
            and artifact['workflow_run']['id'] == RUN_ID and artifact['workflow_run']['head_sha'] == SOURCE_SHA
            and artifact['digest'] == 'sha256:' + ARCHIVE_SHA256, 'Qualified artifact mismatch')
    archive = github_api(f'actions/artifacts/{ARTIFACT_ID}/zip')
    require(sha256(archive) == ARCHIVE_SHA256, 'Artifact archive SHA256 mismatch')
    with zipfile.ZipFile(io.BytesIO(archive)) as zipped:
        require(sorted(zipped.namelist()) == sorted([BINARY, 'musl-build-x64.txt']), 'Unexpected artifact layout')
        binary = zipped.read(BINARY)
    require(sha256(binary) == BINARY_SHA256 and binary[:6] == b'\x7fELF\x02\x01'
            and binary[18:20] == b'\x3e\x00', 'Qualified ELF64 x64 binary mismatch')
    source = json.loads(github_api(f'contents/npm/linux-x64-musl/package.json?ref={SOURCE_SHA}'))
    qualified_manifest = json.loads(base64.b64decode(source['content']))
    manifest = read_json(ROOT / 'npm/linux-x64-musl/package.json')
    require(manifest == qualified_manifest, 'Workflow sidecar manifest differs from qualified source')
    readme_source = json.loads(github_api(f'contents/npm/linux-x64-musl/README.md?ref={SOURCE_SHA}'))
    readme = (ROOT / 'npm/linux-x64-musl/README.md').read_bytes()
    require(readme == base64.b64decode(readme_source['content']), 'Workflow sidecar README differs from qualified source')
    require(manifest['name'] == PACKAGE and manifest['main'] == BINARY and manifest['cpu'] == ['x64']
            and manifest['os'] == ['linux'] and manifest['libc'] == ['musl']
            and not manifest.get('scripts'), 'Unexpected sidecar manifest')
    require(version_metadata() is None, 'Bootstrap version already exists; refusing another publish')
    require(checked_command(['npm', '--version']).decode().strip() == '12.2.1', 'Expected npm12.2.1')
    require(checked_command(['git', 'rev-parse', 'HEAD'], cwd=ROOT).decode().strip() == os.environ['GITHUB_SHA'], 'Checkout SHA mismatch')
    (directory / BINARY).write_bytes(binary)
    packed = directory / 'private' / 'packed'
    packed.mkdir()
    manifest['version'] = VERSION
    # Match bootstrap-native-package.mjs exactly, including package.json key order.
    (packed / 'package.json').write_text(json.dumps(manifest, indent=2) + '\n')
    (packed / 'README.md').write_bytes(readme)
    (packed / BINARY).write_bytes(binary)
    # Pack before login, with scripts disabled. This receipt must match anonymous readback.
    checked_command(['npm', 'pack', '--json'], cwd=packed, env=npm_environment(directory))
    tarballs = list(packed.glob('*.tgz'))
    require(len(tarballs) == 1, 'Expected exactly one prepared tarball')
    tarball = tarballs[0].read_bytes()
    with tarfile.open(fileobj=io.BytesIO(tarball), mode='r:gz') as archive:
        require(sorted(archive.getnames()) == sorted(['package/package.json', 'package/README.md', 'package/' + BINARY]), 'Unexpected packed files')
    receipt = {'package': PACKAGE, 'version': VERSION, 'tag': 'bootstrap', 'qualified_run_id': RUN_ID,
               'qualified_source_sha': SOURCE_SHA, 'artifact_id': ARTIFACT_ID, 'archive_sha256': ARCHIVE_SHA256,
               'binary_sha256': BINARY_SHA256, 'binary_bytes': len(binary), 'workflow_source_sha': os.environ['GITHUB_SHA'],
               'readme_sha256': sha256(readme),
               'source_note': 'Binary and manifest come from the qualified run; orchestration and bootstrap helper come from the workflow revision.',
               'helper_hashes': {name: sha256((ROOT / 'scripts' / name).read_bytes()) for name in
                                 ['bootstrap-native-package.mjs', 'npm-pack-metadata.mjs', 'bootstrap-npm-github.py']},
               'tarball': {'sha1': hashlib.sha1(tarball).hexdigest(), 'sha256': sha256(tarball),
                           'integrity': 'sha512-' + base64.b64encode(hashlib.sha512(tarball).digest()).decode()},
               'manifest': manifest}
    write_json(directory / 'public' / 'preparation.json', receipt)
    write_json(directory / 'status.json', {'phase': 'prepared'})


def approved_challenge_url(url, phase):
    parsed = urllib.parse.urlparse(url)
    require(parsed.scheme == 'https' and parsed.netloc == 'www.npmjs.com' and not parsed.fragment,
            'Unexpected npm browser challenge origin')
    if phase == 'login':
        query = urllib.parse.parse_qs(parsed.query, strict_parsing=True)
        require(parsed.path == '/login' and list(query) == ['next'] and len(query['next']) == 1
                and re.fullmatch('/login/cli/' + UUID, query['next'][0]), 'Unexpected npm login challenge path')
    else:
        require(phase == 'publish' and not parsed.query and re.fullmatch('/auth/cli/' + UUID, parsed.path),
                'Unexpected npm publish challenge path')
    return url


def capture_challenge(buffer, phase, directory):
    # Do not expose arbitrary URLs or CLI output. doneUrl is never part of this allowlist.
    for match in re.finditer(r'https://www\.npmjs\.com/[^\s\x1b]+(?=[\s\x1b])', buffer):
        candidate = match.group(0)
        if '/login?' not in candidate and '/auth/cli/' not in candidate:
            continue
        url = approved_challenge_url(candidate, phase)
        path = directory / f'{phase}-challenge.json'
        value = {'phase': phase, 'url': url, 'expected_npm_user': EXPECTED_USER,
                 'package': PACKAGE, 'version': VERSION, 'tag': 'bootstrap'}
        if path.exists():
            require(read_json(path) == value, 'Unexpected repeated npm approval challenge')
        else:
            write_json(path, value)


def run_pty(command, phase, directory, env, timeout=900):
    master, slave = pty.openpty()
    process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave,
                               cwd=ROOT, env=env, start_new_session=True)
    os.close(slave)
    deadline = time.monotonic() + timeout
    buffer = ''
    try:
        while True:
            require(time.monotonic() < deadline, f'{phase} browser approval timed out')
            ready, _, _ = select.select([master], [], [], 0.2)
            if ready:
                try:
                    data = os.read(master, 8192)
                except OSError as error:
                    if error.errno != 5:
                        raise
                    data = b''
                if data:
                    buffer = (buffer + data.decode('utf-8', errors='replace'))[-65536:]
                    capture_challenge(buffer, phase, directory)
            if process.poll() is not None:
                # Drain any last output before accepting process termination.
                try:
                    while select.select([master], [], [], 0)[0]:
                        data = os.read(master, 8192)
                        if not data:
                            break
                        buffer = (buffer + data.decode('utf-8', errors='replace'))[-65536:]
                        capture_challenge(buffer, phase, directory)
                except OSError as error:
                    if error.errno != 5:
                        raise
                require(process.returncode == 0, f'{phase} npm command exited {process.returncode}; raw output withheld')
                return
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        os.close(master)


def readback(directory, timeout=600):
    expected = read_json(directory / 'public' / 'preparation.json')
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        metadata = version_metadata()
        if metadata is None:
            time.sleep(5)
            continue
        try:
            tarball = anonymous_get(metadata['dist']['tarball'])
        except urllib.error.HTTPError as error:
            if error.code == 404:
                time.sleep(5)
                continue
            raise
        integrity = 'sha512-' + base64.b64encode(hashlib.sha512(tarball).digest()).decode()
        require(metadata['dist']['shasum'] == expected['tarball']['sha1'] == hashlib.sha1(tarball).hexdigest(), 'Published tarball SHA1 mismatch')
        require(metadata['dist']['integrity'] == expected['tarball']['integrity'] == integrity, 'Published tarball SHA512 mismatch')
        with tarfile.open(fileobj=io.BytesIO(tarball), mode='r:gz') as archive:
            require(sorted(archive.getnames()) == sorted(['package/package.json', 'package/README.md', 'package/' + BINARY]), 'Published package layout mismatch')
            require(all(item.isfile() for item in archive.getmembers()), 'Published tarball contains nonregular files')
            require(json.load(archive.extractfile('package/package.json')) == expected['manifest'], 'Published manifest mismatch')
            require(sha256(archive.extractfile('package/README.md').read()) == expected['readme_sha256'], 'Published README mismatch')
            require(sha256(archive.extractfile('package/' + BINARY).read()) == BINARY_SHA256, 'Published native bytes mismatch')
        packument = json.loads(anonymous_get(REGISTRY + urllib.parse.quote(PACKAGE, safe='@')))
        require(packument['dist-tags'].get('bootstrap') == VERSION, 'Published bootstrap tag mismatch')
        write_json(directory / 'public' / 'readback.json', {'checked_at': datetime.now(timezone.utc).isoformat(),
                   'status': 'verified', 'authentication': 'anonymous', 'package': PACKAGE, 'version': VERSION,
                   'dist': metadata['dist'], 'dist_tags': packument['dist-tags'], 'binary_sha256': BINARY_SHA256,
                   'tarball_sha256': sha256(tarball), 'workflow_source_sha': os.environ['GITHUB_SHA']})
        (directory / 'public' / 'package.tgz').write_bytes(tarball)
        return
    raise RuntimeError('Publish accepted, but anonymous readback timed out; inspect registry before any retry')


def erase_auth(directory):
    config = directory / 'private' / 'npmrc'
    logout = 'no-session'
    if config.exists() and '_authToken=' in config.read_text():
        try:
            result = subprocess.run(['npm', 'logout', '--registry', REGISTRY], env=npm_environment(directory),
                                    cwd=directory, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=20)
            logout = 'revoked' if result.returncode == 0 else 'revocation-failed'
        except (subprocess.TimeoutExpired, OSError):
            logout = 'revocation-failed'
    try:
        if (directory / 'private').exists():
            shutil.rmtree(directory / 'private')
    except OSError:
        pass
    removed = not (directory / 'private').exists()
    write_json(directory / 'public' / 'credential-cleanup.json', {'npm_logout': logout, 'local_credentials_removed': removed})
    require(removed, 'Runner npm credential directory could not be removed')
    require(logout != 'revocation-failed', 'Runner npm session revocation failed; credentials removed locally')


def controller(directory):
    def interrupted(_signal, _frame):
        raise KeyboardInterrupt()
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    os.umask(0o077)
    try:
        env = npm_environment(directory)
        write_json(directory / 'status.json', {'phase': 'login'})
        run_pty(['npm', 'login', '--auth-type=web', '--browser=false', '--registry', REGISTRY], 'login', directory, env)
        user = checked_command(['npm', 'whoami', '--registry', REGISTRY], env=env, cwd=directory).decode().strip()
        require(user == EXPECTED_USER, 'Authenticated npm account does not match expected account')
        write_json(directory / 'public' / 'identity.json', {'npm_user': user})
        write_json(directory / 'status.json', {'phase': 'publish'})
        run_pty(['node', str(ROOT / 'scripts/bootstrap-native-package.mjs'), '--package-dir', str(ROOT / 'npm/linux-x64-musl'),
                 '--binary', str(directory / BINARY), '--version', VERSION, '--dist-tag', 'bootstrap', '--publish'],
                'publish', directory, env)
        write_json(directory / 'status.json', {'phase': 'readback', 'npm_exit_code': 0})
        # A recent browser approval can allow the publish without a second challenge.
        if not (directory / 'publish-challenge.json').exists():
            write_json(directory / 'publish-challenge.json', {'phase': 'publish', 'url': None, 'status': 'accepted-without-additional-challenge'})
        readback(directory)
        write_json(directory / 'status.json', {'phase': 'complete', 'npm_exit_code': 0, 'readback': 'verified'})
    except BaseException as error:
        # Messages above are intentionally fixed diagnostics, never npm raw output.
        write_json(directory / 'status.json', {'phase': 'failed', 'error': str(error) if isinstance(error, RuntimeError) else type(error).__name__})
    finally:
        try:
            erase_auth(directory)
        except RuntimeError as error:
            write_json(directory / 'status.json', {'phase': 'failed', 'error': str(error)})
        write_json(directory / 'public' / 'status.json', read_json(directory / 'status.json'))


def await_phase(directory, phase, timeout=930):
    deadline = time.monotonic() + (1530 if phase == 'complete' else timeout)
    while time.monotonic() < deadline:
        state = read_json(directory / 'status.json')
        require(state['phase'] != 'failed', state.get('error', 'Bootstrap failed'))
        if phase in ['login', 'publish'] and (directory / f'{phase}-challenge.json').exists():
            return
        if phase == 'complete' and state['phase'] == 'complete' and (directory / 'public/credential-cleanup.json').exists():
            cleanup = read_json(directory / 'public/credential-cleanup.json')
            require(cleanup['local_credentials_removed'] and cleanup['npm_logout'] == 'revoked', 'Runner npm cleanup did not succeed')
            return
        time.sleep(0.5)
    raise RuntimeError(f'Timed out awaiting bootstrap {phase}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['prepare', 'start', 'controller', 'await', 'cleanup'])
    parser.add_argument('--phase', choices=['login', 'publish', 'complete'])
    args = parser.parse_args()
    directory = runner_directory()
    if args.command == 'prepare':
        prepare(directory)
    elif args.command == 'start':
        require(read_json(directory / 'status.json')['phase'] == 'prepared', 'Unexpected bootstrap state')
        with open(os.devnull, 'wb') as output:
            process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), 'controller'],
                                       stdin=subprocess.DEVNULL, stdout=output, stderr=output,
                                       start_new_session=True, cwd=ROOT)
        (directory / 'controller.pid').write_text(str(process.pid))
    elif args.command == 'controller':
        controller(directory)
    elif args.command == 'await':
        require(args.phase is not None, 'Expected --phase')
        await_phase(directory, args.phase)
    else:
        pidfile = directory / 'controller.pid'
        if pidfile.exists():
            pid = int(pidfile.read_text())
            cmdline = Path(f'/proc/{pid}/cmdline')
            if cmdline.exists() and str(Path(__file__).resolve()).encode() in cmdline.read_bytes():
                os.kill(pid, signal.SIGTERM)
                for _ in range(60):
                    if not cmdline.exists():
                        break
                    time.sleep(0.5)
                if cmdline.exists():
                    os.kill(pid, signal.SIGKILL)
        if not (directory / 'public/credential-cleanup.json').exists():
            erase_auth(directory)
        for phase in ['login', 'publish']:
            (directory / f'{phase}-challenge.json').unlink(missing_ok=True)
        (directory / BINARY).unlink(missing_ok=True)
        cleanup = read_json(directory / 'public/credential-cleanup.json')
        require(cleanup['local_credentials_removed'] and cleanup['npm_logout'] in ['revoked', 'no-session'],
                'Runner npm cleanup did not succeed; inspect credential-cleanup.json')
    print(f'Bootstrap {args.command} completed; no credentials or raw npm output emitted.')


if __name__ == '__main__':
    main()
