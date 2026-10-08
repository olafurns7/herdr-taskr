#!/usr/bin/env python3
"""Exhaust the hub daemon's file descriptors with idle loopback connections, release them, and check the listener."""
import json, os, resource, signal, socket, sqlite3, subprocess, sys, tempfile, time
from pathlib import Path

BINS = {'Go': Path(sys.argv[1]), 'Rust': Path(sys.argv[2])}
LIMIT = int(sys.argv[3]) if len(sys.argv) > 3 else 64

def limit():
    resource.setrlimit(resource.RLIMIT_NOFILE, (LIMIT, LIMIT))

def ask(port):
    try:
        with socket.create_connection(('127.0.0.1', port), timeout=3) as s:
            s.sendall(f'POST /api/rpc HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n'.encode())
            data = s.recv(4096)
            return data.split(b'\r\n', 1)[0].decode() or '<empty>'
    except OSError as e:
        return f'<{type(e).__name__}: {e}>'

out = {}
with tempfile.TemporaryDirectory(prefix='rev-r2-emfile-') as tmp:
    for kind, binary in BINS.items():
        home = Path(tmp) / kind
        state = home / '.local/state/taskr'; state.mkdir(parents=True)
        (state / 'dashboard.addr').write_text('127.0.0.1:0\n')
        db = home / 'hub.db'
        env = {'PATH': '/usr/bin:/bin', 'HOME': str(home), 'TASKR_DB': str(db), 'LANG': 'C.UTF-8', 'TZ': 'UTC',
               'HERDR_SOCKET_PATH': str(home / 'absent.sock')}
        log = open(home / 'stdout.log', 'wb')
        p = subprocess.Popen([str(binary), 'daemon', '--stay'], env=env, stdout=log, stderr=log, preexec_fn=limit)
        port = None
        end = time.monotonic() + 15
        while time.monotonic() < end and port is None:
            try:
                with sqlite3.connect(db) as c:
                    r = c.execute("select value from meta where key='dashboard_url'").fetchone()
                if r:
                    port = int(r[0].rstrip('/').rsplit(':', 1)[1])
            except sqlite3.Error:
                pass
            time.sleep(.05)
        res = {'before': ask(port)}
        held = []
        for _ in range(LIMIT * 3):
            try:
                held.append(socket.create_connection(('127.0.0.1', port), timeout=1))
            except OSError:
                break
        time.sleep(1.5)
        for s in held:
            s.close()
        res['opened'] = len(held)
        time.sleep(3)
        res['after'] = ask(port)
        res['daemon_alive'] = p.poll() is None
        with sqlite3.connect(db) as c:
            r = c.execute("select value from meta where key='dashboard_url'").fetchone()
        res['dashboard_url_meta'] = r[0] if r else None
        logf = state / 'daemon.log'
        res['daemon_log_tail'] = logf.read_text().splitlines()[-4:] if logf.exists() else []
        p.send_signal(signal.SIGTERM)
        try: p.wait(timeout=20)
        except subprocess.TimeoutExpired: p.kill(); p.wait()
        log.close()
        res['stdout_tail'] = (home / 'stdout.log').read_text(errors='replace').splitlines()[-3:]
        out[kind] = res

for kind, result in out.items():
    assert result['before'].startswith('HTTP/1.1 403'), (kind, result)
    assert result['after'].startswith('HTTP/1.1 403'), (kind, result)
    assert result['daemon_alive'] and result['dashboard_url_meta'], (kind, result)

print(json.dumps(out, indent=2))
