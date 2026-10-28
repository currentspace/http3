"""Qualify the final 0.9.3 prebuild with real stable Safari and trusted TLS."""
import base64
import faulthandler
import json
import os
import pathlib
import subprocess
import time
import urllib.request

faulthandler.enable()
faulthandler.dump_traceback_later(45, repeat=True)
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

root = pathlib.Path('artifacts/safari-latest').resolve()
ca = str(root / 'ca.pem')
session = None
server = None
driver = None
trusted = False

def save(name, value):
    (root / name).write_text(json.dumps(value, indent=2))

def wd(method, path, data=None):
    print('WebDriver:', method, path, flush=True)
    body = None if data is None else json.dumps(data).encode()
    req = urllib.request.Request('http://127.0.0.1:4444' + path, data=body, method=method,
                                 headers={'Content-Type': 'application/json'})
    with opener.open(req, timeout=45) as response:
        value = json.load(response)['value']
    if isinstance(value, dict) and 'error' in value:
        raise RuntimeError(value)
    return value

def run(args, **kwargs):
    print('Running:', args[:3], flush=True)
    return subprocess.run(args, timeout=40, **kwargs)

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
    run(['sudo', 'security', 'add-trusted-cert', '-d', '-r', 'trustRoot',
                    '-k', '/Library/Keychains/System.keychain', ca], check=True)
    trusted = True
    fingerprint = subprocess.check_output(['openssl', 'x509', '-in', ca, '-noout', '-fingerprint', '-sha1'], text=True).strip().split('=')[-1].replace(':', '')
    (root / 'ca-fingerprint.txt').write_text(fingerprint)
    run(['sudo', 'safaridriver', '--enable'], check=True)
    run(['defaults', 'write', 'com.apple.Safari', 'IncludeDevelopMenu', '-bool', 'true'], check=True)
    for preference in ['WebKitDeveloperExtrasEnabledPreferenceKey', 'com.apple.Safari.ContentPageGroupIdentifier.WebKit2DeveloperExtrasEnabled']:
        run(['defaults', 'write', 'com.apple.Safari', preference, '-bool', 'true'], check=True)
    driver = subprocess.Popen(['/usr/bin/safaridriver', '-p', '4444'],
                              stdout=(root / 'safaridriver.log').open('w'), stderr=subprocess.STDOUT)
    for _ in range(5):
        try:
            wd('GET', '/status')
            break
        except Exception as error:
            print('Waiting for driver:', error, flush=True)
            assert driver.poll() is None, 'SafariDriver exited'
            time.sleep(.5)
    value = wd('POST', '/session', {'capabilities': {'alwaysMatch': {
        'browserName': 'safari', 'acceptInsecureCerts': False}}})
    save('safari-session.json', value)
    session = value['sessionId']
    assert value['capabilities']['browserVersion'].startswith('27.2'), value
    assert value['capabilities']['acceptInsecureCerts'] is False, value
    command('POST', '/window/rect', {'width': 1000, 'height': 720, 'x': 0, 'y': 25})
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
        result['layout'] = evaluate("return {text:document.body.innerText,scrollX,scrollY,width:innerWidth,height:innerHeight,children:[...document.body.children].filter(e=>e.tagName!=='SCRIPT').map(e=>({tag:e.tagName,rect:e.getBoundingClientRect().toJSON(),color:getComputedStyle(e).color,display:getComputedStyle(e).display,visibility:getComputedStyle(e).visibility}))}")
        save('safari-' + phase + '.json', result)
        time.sleep(2)
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
    # Use a normal window for manual Inspector capture after WebDriver assertions.
    command('DELETE', '')
    session = None
    run(['osascript', '-e', 'tell application "Safari" to quit'], check=True)
    time.sleep(2)
    run(['open', '-a', 'Safari', endpoint], check=True)
    time.sleep(5)
    run(['screencapture', '-x', str(root / 'safari-manual-page.png')], check=True)
    script = '''tell application "System Events"
tell process "Safari"
set frontmost to true
keystroke "," using command down
delay 2
click button "Advanced" of toolbar 1 of front window
delay 1
repeat with element in entire contents of front window
try
if role of element is "AXCheckBox" then
set labelText to ""
try
set labelText to name of element as text
end try
try
set labelText to labelText & " " & description of element as text
end try
if labelText contains "web developers" then
if value of element is 1 then click element
delay 0.5
click element
end if
end if
end try
end repeat
keystroke "w" using command down
delay 1
keystroke "i" using {command down, option down}
delay 2
set descriptions to {}
repeat with element in entire contents of front window
set labelText to ""
try
set labelText to name of element as text
end try
try
set labelText to labelText & " | " & description of element as text
end try
set end of descriptions to labelText
try
if labelText contains "Network" then click element
end try
end repeat
keystroke "r" using command down
delay 3
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
    run(['screencapture', '-x', str(root / 'final-desktop.png')], check=False)
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
        run(['sudo', 'security', 'delete-certificate', '-Z', fingerprint,
                        '/Library/Keychains/System.keychain'], check=True)
        certs = subprocess.check_output(['security', 'find-certificate', '-a', '-Z', '/Library/Keychains/System.keychain'], text=True)
        assert fingerprint not in certs, 'Temporary certificate remains installed'
        save('trust-cleanup.json', {'certificateRemoved': True, 'verifiedAbsentByFingerprint': True,
                                   'scope': 'Disposable GitHub-hosted VM; VM teardown removes its admin trust record'})
