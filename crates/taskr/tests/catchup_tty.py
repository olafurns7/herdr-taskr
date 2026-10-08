#!/usr/bin/env python3
"""The permitted TTY glance fallback must match its non-TTY snapshot stream."""
import errno
import os
from pathlib import Path
import pty
import subprocess
import sys
import tempfile

binary = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix='taskr-catchup-tty-') as tmp:
    env = {'PATH': '/usr/bin:/bin', 'HOME': tmp, 'TASKR_DB': tmp + '/ledger.db',
           'HERDR_SOCKET_PATH': tmp + '/absent.sock', 'TERM': 'xterm',
           'TASKR_FROZEN_NOW': '2026-10-08T00:00:00Z', 'LANG': 'C.UTF-8', 'TZ': 'UTC'}
    pipe = subprocess.run([str(binary), 'glance', '--watch'], env=env, capture_output=True, timeout=5)
    master, slave = pty.openpty()
    try:
        tty = subprocess.run([str(binary), 'glance', '--watch'], env=env,
                             stdin=slave, stdout=slave, stderr=subprocess.PIPE, timeout=5)
        os.close(slave)
        slave = None
        data = bytearray()
        while True:
            try:
                chunk = os.read(master, 8192)
                if not chunk:
                    break
                data.extend(chunk)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                break
        assert tty.returncode == pipe.returncode == 0, (tty, pipe)
        assert tty.stderr == pipe.stderr == b'', (tty.stderr, pipe.stderr)
        assert bytes(data).replace(b'\r\n', b'\n') == pipe.stdout, (data, pipe.stdout)
        assert b'\x1b' not in data
        print('PASS: TTY exit 0, no 125/alternate-screen escapes, same snapshot bytes as pipe')
    finally:
        if slave is not None:
            os.close(slave)
        os.close(master)
