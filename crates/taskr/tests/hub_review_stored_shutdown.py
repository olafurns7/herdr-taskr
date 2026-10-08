#!/usr/bin/env python3
"""Stop a hub while a stored write waits on the SQLite lock; inspect the stored request and replay its key."""
import json, os, signal, socket, sqlite3, subprocess, sys, tempfile, time, select
from pathlib import Path
sys.path.insert(0, sys.argv[1])  # crates/taskr/tests
from hub_cell import Cell, RustHub

GO, RUST = Path(sys.argv[2]), Path(sys.argv[3])

def post(port, req, wait=True):
    body = json.dumps(req)
    s = socket.create_connection(('::1', port), timeout=30)
    s.sendall(f'POST /api/rpc HTTP/1.1\r\nHost: [::1]:{port}\r\nContent-Type: application/json\r\nX-Taskr-RPC: 1\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n{body}'.encode())
    if not wait:
        return s
    data = b''
    while True:
        chunk = s.recv(65536)
        if not chunk:
            break
        data += chunk
    s.close()
    return data.decode(errors='replace').split('\r\n\r\n', 1)[-1]

def row(db, key):
    with sqlite3.connect(db) as c:
        return c.execute('select state, exit, stdout from requests where key=?', (key,)).fetchone()

def notes(db, text):
    with sqlite3.connect(db) as c:
        return c.execute("select count(*) from events where kind='note' and summary=?", (text,)).fetchone()[0]

def restart(cell, kind):
    if kind == 'Rust':
        p = subprocess.Popen([str(RUST), '--contract-hub'], env=cell.hub_env, stdout=subprocess.PIPE, stderr=cell.log)
        assert select.select([p.stdout], [], [], 15)[0]
        url = p.stdout.readline().decode().strip()
        return p, int(url.rsplit(':', 1)[1])
    p = subprocess.Popen([str(GO), 'daemon', '--stay'], env=cell.hub_env, stdout=cell.log, stderr=cell.log)
    end = time.monotonic() + 15
    while time.monotonic() < end:
        try:
            with sqlite3.connect(cell.db) as c:
                r = c.execute("select value from meta where key='dashboard_url'").fetchone()
            if r:
                port = int(r[0].split(':')[-1].strip('/'))
                socket.create_connection(('::1', port), timeout=.2).close()
                return p, port
        except (sqlite3.Error, OSError):
            pass
        time.sleep(.05)
    raise AssertionError('go restart failed')

out = {}
with tempfile.TemporaryDirectory(prefix='rev-r2-stored-') as tmp:
    for kind, cls in (('Go', Cell), ('Rust', RustHub)):
        place = Path(tmp) / kind; place.mkdir()
        cell = cls(GO, RUST, place)
        try:
            root = json.loads(cell.record(['new', 'probe-root', '--role', 'orchestrator']).stdout)['task_id']
            port = int(cell.url.rsplit(':', 1)[1])
            key = f'probe-release-{kind.lower()}'
            text = f'shutdown write {kind}'
            req = {'argv': ['note', text, '--as', str(root)], 'cwd': '/', 'request_key': key}
            blocker = sqlite3.connect(cell.db, timeout=0, isolation_level=None)
            s = post(port, req, wait=False)
            # Take the write lock only after the hub's claim commits, so only the child's write waits.
            end = time.monotonic() + 5
            seen = None
            while time.monotonic() < end:
                try:
                    seen = blocker.execute('select state from requests where key=?', (key,)).fetchone()
                    if seen:
                        blocker.execute('begin immediate')
                        break
                except sqlite3.OperationalError:
                    pass
            claimed_then_locked = bool(seen) and notes(cell.db, text) == 0
            time.sleep(1.0)
            t0 = time.monotonic()
            cell.hub.send_signal(signal.SIGTERM)
            # Release the lock shortly after the stop signal: no outside writer remains, as in a real stop.
            time.sleep(0.3)
            if blocker.in_transaction: blocker.execute('rollback')
            try:
                cell.hub.wait(timeout=20)
            except subprocess.TimeoutExpired:
                cell.hub.kill(); cell.hub.wait()
            stop_s = round(time.monotonic() - t0, 3)
            s.settimeout(5)
            try:
                first = s.recv(65536).decode(errors='replace').split('\r\n\r\n', 1)[-1]
            except OSError as e:
                first = f'<{e}>'
            s.close()
            blocker.execute('rollback') if blocker.in_transaction else None; blocker.close()
            after_stop = row(cell.db, key)
            p, port2 = restart(cell, kind)
            try:
                replay = post(port2, req)
                after_replay = row(cell.db, key)
                out[kind] = {'claimed_then_locked': claimed_then_locked, 'stop_seconds': stop_s, 'first_reply': first, 'row_after_stop': after_stop,
                             'replay_reply': replay, 'row_after_replay': after_replay, 'note_events': notes(cell.db, text)}
            finally:
                p.send_signal(signal.SIGTERM)
                try: p.wait(timeout=20)
                except subprocess.TimeoutExpired: p.kill(); p.wait()
                if kind == 'Rust': p.stdout.close()
        finally:
            try: cell.hub.kill()
            except Exception: pass
            try: cell.close()
            except Exception: pass

for kind, result in out.items():
    assert result['claimed_then_locked'], (kind, result)
    state, code, _ = result['row_after_stop']
    assert state == 'running' or (state == 'done' and code == 0), (kind, result)
    reply = json.loads(result['replay_reply'])
    if state == 'running':
        assert reply['exit'] == 5 and 'outcome unknown' in reply['stdout'], (kind, result)
    else:
        assert reply['exit'] == 0 and result['note_events'] == 1, (kind, result)
    assert result['note_events'] <= 1 and result['stop_seconds'] < 5, (kind, result)

print(json.dumps(out, indent=2))
