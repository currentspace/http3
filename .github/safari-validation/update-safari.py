"""Install only the stable Safari update on this disposable hosted Mac."""
import pathlib
import re
import subprocess

out = pathlib.Path('artifacts/safari-latest')
out.mkdir(parents=True, exist_ok=True)

def version():
    return subprocess.check_output(['/usr/bin/safaridriver', '--version'], text=True).strip()

before = version()
(out / 'safari-before.txt').write_text(before)
if not re.search(r'\b27\.2\b', before):
    result = subprocess.run(['sudo', 'softwareupdate', '--list'], capture_output=True, text=True, timeout=600)
    listing = result.stdout + result.stderr
    (out / 'softwareupdate-list.txt').write_text(listing)
    print(listing, flush=True)
    result.check_returncode()
    labels = re.findall(r'^\s*\* Label: (Safari[^\n]*27\.2[^\n]*)$', listing, re.M)
    assert len(labels) == 1, f'Expected one Safari-only 27.2 update; found {labels}'
    subprocess.run(['sudo', 'softwareupdate', '--install', labels[0], '--verbose'], check=True, timeout=1200)
after = version()
(out / 'safari-version.txt').write_text(after)
(out / 'macos-version.txt').write_text(subprocess.check_output(['sw_vers'], text=True))
assert re.search(r'\b27\.2\b', after), after
print(after, flush=True)
