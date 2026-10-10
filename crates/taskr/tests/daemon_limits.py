#!/usr/bin/env python3
"""Linux process proof: a daemon inherits soft64/hard256 and raises only its soft limit."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools/contract'))
import golden
import argparse
import json
import resource
import select
import subprocess
import tempfile
import time


def check(binary, home, expected_soft):
    state = home / '.local/state/taskr'
    state.mkdir(parents=True)
    (state / 'dashboard.addr').write_text('off\n')
    fake = home / 'bin'; fake.mkdir()
    for name in ('herdr', 'tailscale'):
        path = fake / name
        path.write_text('#!/bin/sh\nexit 2\n')
        path.chmod(0o755)
    env = {'TASKR_TMP_BASE': str(home / 'taskr-tmp'), 'HOME': str(home), 'TASKR_DB': str(home / 'ledger.db'),
           'PATH': f'{fake}:/usr/bin:/bin', 'HERDR_SOCKET_PATH': str(home / 'absent.sock'),
           'LANG': 'C.UTF-8', 'TZ': 'UTC'}

    def lower():
        resource.setrlimit(resource.RLIMIT_NOFILE, (64, 256))

    with (home / 'output.log').open('wb') as output:
        daemon = subprocess.Popen([str(binary), 'daemon', '--stay'], env=env,
                                  stdout=output, stderr=output, preexec_fn=lower)
        try:
            deadline = time.monotonic() + 10
            observed = None
            while time.monotonic() < deadline:
                assert daemon.poll() is None, (home / 'output.log').read_text()
                for line in Path(f'/proc/{daemon.pid}/limits').read_text().splitlines():
                    if line.startswith('Max open files '):
                        observed = tuple(map(int, line.split()[3:5]))
                if observed == (expected_soft, 256):
                    return {'inherited': [64, 256], 'observed': list(observed)}
                select.select([], [], [], .025)
            raise AssertionError(('soft limit was not raised', observed, (home / 'output.log').read_text()))
        finally:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill(); daemon.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--go', required=True, type=Path)
    parser.add_argument('--rust', required=True, type=Path)
    parser.add_argument('--out', type=Path)
    args = golden.parse(parser, __file__)
    original = resource.getrlimit(resource.RLIMIT_NOFILE)
    with tempfile.TemporaryDirectory(prefix='taskr-daemon-limits-') as tmp:
        # This host's Go runtime reserves one descriptor to detect external prlimit changes.
        # Rust follows the explicit daemon brief: raise to the hard limit itself.
        result = {name: check(binary.resolve(), Path(tmp).resolve() / name, soft)
                  for name, binary, soft in (('Go', args.go, 256 if golden.session and golden.session.oracle_is_rust else 255), ('Rust', args.rust, 256))}
    assert resource.getrlimit(resource.RLIMIT_NOFILE) == original
    if args.out:
        args.out.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()

    golden.finish()
