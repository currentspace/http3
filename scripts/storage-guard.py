#!/usr/bin/env python3
"""Run a local build with disk headroom checks and process-group cancellation.

Example: python3 scripts/storage-guard.py --docker --reserve-gib 10 -- docker build ...
Watch the workspace, home (including Docker Desktop), temp, and any --path.
Docker inventory is recorded before and after a Docker operation. No pruning.
"""

import argparse
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def sample(path):
    # df accounts for APFS space shared with other volumes; total filesystem
    # blocks alone can substantially understate the effective used percentage.
    line = subprocess.check_output(["df", "-Pk", str(path)], text=True).splitlines()[-1]
    fields = line.split()
    used, available = int(fields[2]) * 1024, int(fields[3]) * 1024
    return used, available


def check(paths, limit, reserve=0, report=False):
    for path in paths:
        used, available = sample(path)
        capacity = used + available
        projected = 100 * (used + reserve) / capacity
        if report:
            print(f"storage {path}: {100 * used / capacity:.2f}% used, "
                  f"{available / 1024**3:.1f} GiB available", flush=True)
        if projected >= limit or available <= reserve:
            raise RuntimeError(f"{path}: projected usage {projected:.2f}% "
                               f"reaches {limit}% stop threshold")


def docker_inventory():
    subprocess.run(["docker", "system", "df"], check=True, timeout=30)


def stop(process):
    # Signal the whole build tree, including shells and compiler children.
    # Docker CLI interruption also requests cancellation of its daemon build.
    for sig, timeout in [(signal.SIGINT, 10), (signal.SIGTERM, 5), (signal.SIGKILL, 2)]:
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            break
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            continue
        # A shell may exit before its children; continue signalling the group.
    process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--path", action="append", default=[])
    parser.add_argument("--limit", type=float, default=94)
    parser.add_argument("--reserve-gib", type=float, default=2)
    parser.add_argument("--docker", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if not 0 < args.limit < 95 or args.reserve_gib < 0:
        parser.error("limit must be below 95%; reserve must be nonnegative")
    command = args.command
    if command[:1] == ["--"]:
        command = command[1:]
    paths = list(dict.fromkeys([str(Path.cwd()), str(Path.home()),
                               os.environ.get("TMPDIR", "/tmp"), *args.path]))
    process = None
    result = 0
    try:
        check(paths, args.limit, args.reserve_gib * 1024**3, report=True)
        if args.docker:
            docker_inventory()
        if command:
            process = subprocess.Popen(command, start_new_session=True)
            next_report = time.monotonic() + 60
            while process.poll() is None:
                report = time.monotonic() >= next_report
                check(paths, args.limit, report=report)
                if report:
                    next_report = time.monotonic() + 60
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pass
            result = process.returncode
    except (RuntimeError, OSError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f"storage guard: {error}", file=sys.stderr, flush=True)
        if process is not None:
            stop(process)
        result = 125
    finally:
        try:
            check(paths, args.limit, report=True)
            if args.docker:
                docker_inventory()
        except (RuntimeError, OSError, subprocess.SubprocessError) as error:
            print(f"storage guard final check: {error}", file=sys.stderr, flush=True)
            result = 125
    return result


if __name__ == "__main__":
    sys.exit(main())
