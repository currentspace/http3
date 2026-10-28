"""Qualify the final 0.9.3 prebuild with real stable Safari and trusted TLS."""
import base64
import json
import os
import pathlib
import subprocess
import time
import urllib.request

root = pathlib.Path('artifacts/safari-latest').resolve()
ca = str(root / 'ca.pem')
session = None
server = None
driver = None
trusted = False

def save(name, value):
    (root / name).write_text(json.dumps(value, indent=2))

def wd(method, path, data=None):
    body = None if data is None else json.dumps(data).encode()
    req = urllib.request.Request('http://127.0.0.1:4444' + path, data=body, method=method,
                                 headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=60) as response:
        value = json.load(response)['value']
    if isinstance(value, dict) and 'error' in value:
        raise RuntimeError(value)
    return value

def command(method, path, data=None):
    return wd(method, '/session/' + session + path, data)

def evaluate(script):
    return command('POST', '/execute/sync', {'script': script, 'args': []})

try:
    server = subprocess.Popen(['node', '.github/safari-validation/server.mjs'],
                              stdout=(root / 'server.log').open('w'), stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 30
    while not (root / 'endpoint.json').exists() and time.monotonic() < deadline:
        assert server.poll() is None, 'Fixture exited before listening'
        time.sleep(.2)
    endpoint = json.loads((root / 'endpoint.json').read_text())['url']
    subprocess.run(['sudo', 'security', 'add-trusted-cert', '-d', '-r', 'trustRoot',
                    '-k', '/Library/Keychains/System.keychain', ca], check=True)
    trusted = True
    fingerprint = subprocess.check_output(['openssl', 'x509', '-in', ca, '-noout', '-fingerprint', '-sha1'], text=True).strip().split('=')[-1].replace(':', '')
    (root / 'ca-fingerprint.txt').write_text(fingerprint)
    subprocess.run(['sudo', 'safaridriver', '--enable'], check=True)
    subprocess.run(['defaults', 'write', 'com.apple.Safari', 'IncludeDevelopMenu', '-bool', 'true'], check=True)
    driver = subprocess.Popen(['/usr/bin/safaridriver', '-p', '4444'],
                              stdout=(root / 'safaridriver.log').open('w'), stderr=subprocess.STDOUT)
    for _ in range(60):
        try:
            wd('GET', '/status')
            break
        except Exception:
            time.sleep(.5)
    value = wd('POST', '/session', {'capabilities': {'alwaysMatch': {
        'browserName': 'safari', 'acceptInsecureCerts': False, 'safari:automaticInspection': True}}})
    save('safari-session.json', value)
    session = value['sessionId']
    assert value['capabilities']['browserVersion'].startswith('27.2'), value
    assert value['capabilities']['acceptInsecureCerts'] is False, value
    command('POST', '/window/rect', {'width': 1400, 'height': 1100})
    results = []
    for phase in ['initial', 'reload']:
        command('POST', '/url', {'url': endpoint}) if phase == 'initial' else command('POST', '/refresh', {})
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            result = evaluate('return window.result || {complete:false}')
            if result.get('complete'):
                break
            time.sleep(.5)
        result.update(phase=phase, userAgent=evaluate('return navigator.userAgent'),
                      secureContext=evaluate('return isSecureContext'))
        save('safari-' + phase + '.json', result)
        (root / ('safari-' + phase + '.png')).write_bytes(base64.b64decode(command('GET', '/screenshot')))
        assert result.get('complete') and result['secureContext'] and not result['errors'], result
        assert result['loads'] == (1 if phase == 'initial' else 2), result
        assert [e['lastEventId'] for e in result['events']] == ['1', '2'], result
        results.append(result)
        print(phase, 'PASS', json.dumps(result), flush=True)
    for protocol, expected in [('http3-only', '3'), ('http2', '2')]:
        result = subprocess.run([os.environ['QUALIFICATION_CURL'], '--' + protocol, '--cacert', ca,
                                 '--max-time', '30', '-sS', '-D', '-', '-w', '\nHTTP_VERSION=%{http_version}\n',
                                 endpoint + '/api/hello'], capture_output=True, text=True)
        (root / ('curl-' + protocol + '.txt')).write_text(result.stdout + result.stderr)
        assert result.returncode == 0 and 'HTTP_VERSION=' + expected in result.stdout, result
        print(protocol, 'PASS', flush=True)
    save('safari-validation.json', {'functionalPassed': True, 'endpoint': endpoint, 'results': results,
                                   'artifactSource': '19dafa4001851b73a268078b377cd551d78200c7',
                                   'artifactRun': 40758754002})
    # Capture the actual Inspector separately; image contents require human/visual review.
    script = '''tell application "System Events"
tell process "Safari"
set frontmost to true
keystroke "i" using {command down, option down}
delay 2
set descriptions to {}
repeat with element in entire contents of front window
try
set end of descriptions to ((role of element as text) & " | " & (name of element as text) & " | " & (description of element as text))
if name of element is "Network" then click element
end try
end repeat
return descriptions
end tell
end tell'''
    ui = subprocess.run(['osascript', '-e', script], capture_output=True, text=True, timeout=40)
    (root / 'inspector-ui.txt').write_text(ui.stdout + ui.stderr)
    time.sleep(2)
    shot = subprocess.run(['screencapture', '-x', str(root / 'safari-inspector.png')], capture_output=True, text=True)
    save('inspector-capture-status.json', {'uiExit': ui.returncode, 'screenshotExit': shot.returncode,
                                          'requiresVisualReview': True})
finally:
    if session:
        try:
            wd('DELETE', '/session/' + session)
        except Exception as error:
            print('Session cleanup:', error)
    for proc in [driver, server]:
        if proc and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
    if trusted:
        subprocess.run(['sudo', 'security', 'remove-trusted-cert', '-d', ca], check=True)
        subprocess.run(['sudo', 'security', 'delete-certificate', '-Z', fingerprint,
                        '/Library/Keychains/System.keychain'], check=True)
        save('trust-cleanup.json', {'temporaryTrustRemoved': True, 'certificateRemoved': True})
