#!/usr/bin/env python3
"""Real PTY refresh, resize and signal checks; pipes retain one frame."""
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
from hub_cell import RustHub
from net_cell import InterruptProxy

CLEAR = b'\x1b[H\x1b[2J'


def check(binary, env, label):
    pipe = subprocess.run([str(binary), 'glance', '--watch', '--every', '1s'],
                          env=env, capture_output=True, timeout=5)
    assert pipe.returncode == 0 and not pipe.stderr and b'\x1b' not in pipe.stdout, pipe
    for sig in (signal.SIGINT, signal.SIGTERM):
        master, slave = pty.openpty()
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 40, 0, 0))
        p = subprocess.Popen([str(binary), 'glance', '--watch', '--every', '1s'], env=env,
                             stdin=slave, stdout=slave, stderr=subprocess.PIPE)
        os.close(slave)
        data = bytearray()
        def until(predicate, timeout=5):
            deadline = time.monotonic() + timeout
            while not predicate(bytes(data)):
                assert p.poll() is None, (label, p.returncode, p.stderr.read(), data)
                remaining = deadline - time.monotonic()
                assert remaining > 0 and select.select([master], [], [], remaining)[0], (label, data)
                data.extend(os.read(master, 8192))
        try:
            until(lambda d: ('─' * 40).encode() in d)
            assert data.startswith(CLEAR), data
            root = subprocess.run([str(binary), '--json', 'new', f'tty-live-{sig.value}',
                                   '--role', 'orchestrator', '--pane', 'wDemo:p1'],
                                  env=env, capture_output=True, timeout=5)
            assert root.returncode == 0, root
            until(lambda d: d.count(CLEAR) >= 2 and f'tty-live-{sig.value}'.encode() in d.split(CLEAR)[-1])
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 96, 0, 0))
            until(lambda d: ('─' * 96).encode() in d)
            started = time.monotonic()
            p.send_signal(sig)
            _, err = p.communicate(timeout=2)
            assert p.returncode == 0 and not err, (label, sig, p.returncode, err)
            assert time.monotonic() - started < 1
            assert b'\x1b[?1049' not in data, data
            print(f'PASS: {label}, refresh, 40→96 resize, {sig.name} exit 0')
        finally:
            if p.poll() is None:
                p.kill(); p.communicate(timeout=5)
            os.close(master)


if __name__ == '__main__':
    binary = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix='taskr-catchup-tty-') as tmp:
        env = {'PATH': '/usr/bin:/bin', 'HOME': tmp, 'TASKR_DB': tmp + '/ledger.db',
               'HERDR_SOCKET_PATH': tmp + '/absent.sock', 'TERM': 'xterm',
               'TASKR_FROZEN_NOW': '2026-10-08T00:00:00Z', 'LANG': 'C.UTF-8', 'TZ': 'UTC'}
        check(binary, env, 'local')
    with tempfile.TemporaryDirectory(prefix='taskr-catchup-tty-rpc-') as tmp:
        cell = RustHub(binary, binary, tmp)
        try:
            check(binary, {**cell.client_env, 'TERM': 'xterm'}, 'RPC')
            # Interrupt a stalled snapshot, before any frame can be drawn.
            proxy = InterruptProxy(cell.url, before=True)
            cell.server_file.write_text(proxy.url + '\n')
            master, slave = pty.openpty()
            p = subprocess.Popen([str(binary), 'glance', '--watch', '--every', '1s'],
                                 env=cell.client_env, stdin=slave, stdout=slave, stderr=subprocess.PIPE)
            os.close(slave)
            try:
                assert proxy.hit.wait(5) and proxy.error is None, proxy.error
                p.send_signal(signal.SIGTERM)
                _, err = p.communicate(timeout=3)
                assert p.returncode == 0 and not err, (p.returncode, err)
                print('PASS: SIGTERM during stalled RPC snapshot exits 0 within its request budget')
            finally:
                if p.poll() is None:
                    p.kill(); p.communicate(timeout=5)
                os.close(master); proxy.close()
                cell.server_file.write_text(cell.url + '\n')
            assert not (cell.client_home / '.local/state/taskr/taskr.db').exists()
        finally:
            cell.close()

# A tall view retains its header within the terminal; dumb terminals get one frame.
with tempfile.TemporaryDirectory(prefix='taskr-tty-height-') as tmp:
    env = {'PATH': '/usr/bin:/bin', 'HOME': tmp, 'TASKR_DB': tmp + '/ledger.db',
           'HERDR_SOCKET_PATH': tmp + '/absent.sock', 'TERM': 'xterm', 'LANG': 'C.UTF-8', 'TZ': 'UTC'}
    for i in range(30):
        subprocess.run([str(binary), '--json', 'new', f'camp-{i:02d}', '--role', 'orchestrator', '--pane', f'wDemo{i}:p1'],
                       env=env, capture_output=True, check=True)
    for term in ('xterm', 'dumb'):
        master, slave = pty.openpty()
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 20, 46, 0, 0))
        p = subprocess.Popen([str(binary), 'glance', '--watch', '--every', '1s'], env={**env, 'TERM': term},
                             stdin=slave, stdout=slave, stderr=subprocess.PIPE)
        data = bytearray()
        try:
            deadline = time.monotonic() + 4
            while time.monotonic() < deadline and (term == 'dumb' or data.count(CLEAR) < 2):
                if not select.select([master], [], [], .2)[0]:
                    if term == 'dumb': break
                    continue
                data.extend(os.read(master, 65536))
            if term == 'dumb':
                p.wait(timeout=5)
                assert p.returncode == 0
                assert CLEAR not in data and len(data.splitlines()) > 20, data
            else:
                frame = bytes(data).split(CLEAR)[1]
                assert frame.startswith('taskr'.encode()) and len(frame.splitlines()) <= 20, frame
                assert not frame.endswith(b'\n'), frame
                p.terminate(); p.wait(timeout=3)
                assert p.returncode == 0
            print(f'PASS: {term}, 46x20 height clipping / one-shot dumb mode')
        finally:
            if p.poll() is None: p.kill(); p.wait(timeout=3)
            os.close(slave); os.close(master); p.stderr.close()

# A failed remote fetch keeps the last good frame and remains alive until signalled.
with tempfile.TemporaryDirectory(prefix='taskr-tty-error-') as tmp:
    cell = RustHub(binary, binary, tmp)
    master, slave = pty.openpty()
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 12, 96, 0, 0))
    root = subprocess.run([str(binary), '--json', 'new', 'last-good-campaign', '--role', 'orchestrator', '--pane', 'wDemo:p1'],
                          env=cell.client_env, capture_output=True, check=True)
    p = subprocess.Popen([str(binary), 'glance', '--watch', '--every', '1s'], env={**cell.client_env, 'TERM': 'xterm'},
                         stdin=slave, stdout=slave, stderr=subprocess.PIPE)
    data = bytearray()
    def read_until(predicate, timeout=5):
        deadline = time.monotonic()+timeout
        while not predicate(bytes(data)):
            assert p.poll() is None, (p.returncode, data)
            assert select.select([master], [], [], max(0, deadline-time.monotonic()))[0], data
            data.extend(os.read(master, 65536))
    try:
        read_until(lambda d: b'last-good-campaign' in d)
        cell.close()
        read_until(lambda d: len(d.split(CLEAR)) >= 3 and b'taskr:' in d.split(CLEAR)[-1] and b'last-good-campaign' in d.split(CLEAR)[-1])
        frame = bytes(data).split(CLEAR)[-1]
        assert frame.startswith(b'taskr:') and len(frame.splitlines()) <= 12, frame
        assert p.poll() is None
        p.terminate(); p.wait(timeout=3)
        assert p.returncode == 0
        print('PASS: failed fetch keeps the last good campaign, clips error+frame to 12 rows, continues and exits on SIGTERM')
    finally:
        if p.poll() is None: p.kill(); p.wait(timeout=3)
        os.close(slave); os.close(master); p.stderr.close(); cell.close()
