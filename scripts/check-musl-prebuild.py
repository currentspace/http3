#!/usr/bin/env python3
"""Check a musl addon before testing it in a clean Alpine Node runtime."""
import re
import subprocess
import sys


def check(binary):
    def readelf(*args):
        return subprocess.run(['readelf', '--wide', *args, binary], check=True,
                              capture_output=True, text=True).stdout

    expected = {'x64': ('Advanced Micro Devices X86-64', 'libc.musl-x86_64.so.1'),
                'arm64': ('AArch64', 'libc.musl-aarch64.so.1')}
    arch = re.search(r'\.linux-(x64|arm64)-musl\.node$', binary)
    if not arch:
        raise ValueError(f'{binary}: not a supported musl artifact name')
    machine, libc = expected[arch.group(1)]
    header = readelf('--file-header')
    if not re.search(r'Machine:\s+' + re.escape(machine) + r'\s*$', header, re.M):
        raise ValueError(f'{binary}: wrong ELF machine; expected {machine}')
    if not re.search(r'Type:\s+DYN\b', header):
        raise ValueError(f'{binary}: expected an ELF shared object')
    needed = set(re.findall(r'\(NEEDED\).*\[([^]]+)\]', readelf('--dynamic')))
    allowed = {libc, 'libgcc_s.so.1', 'libstdc++.so.6'}
    if libc not in needed or needed - allowed:
        raise ValueError(f'{binary}: unexpected runtime dependencies: {sorted(needed)}')
    # Alpine libgcc legitimately versions its frame-registration helpers as
    # GLIBC_2.0. Check the provider, not just the spelling of a version tag.
    provider = None
    for line in readelf('--version-info').splitlines():
        match = re.search(r'File: (\S+)', line)
        if match:
            provider = match.group(1)
        for version in re.findall(r'Name: (GLIBC_\S+)', line):
            if provider != 'libgcc_s.so.1' or version != 'GLIBC_2.0':
                raise ValueError(f'{binary}: contains a glibc symbol dependency from {provider}: {version}')
    print(f'{binary}: {machine}; musl runtime dependencies {sorted(needed)}')


if __name__ == '__main__':
    if len(sys.argv) < 2:
        raise SystemExit('usage: check-musl-prebuild.py <http3.linux-ARCH-musl.node> [...]')
    for path in sys.argv[1:]:
        check(path)
