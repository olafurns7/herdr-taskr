#!/usr/bin/env python3
"""Replace the running Rust hub's binary the way install.sh does (mv -f over it), then send RPCs."""
import json, os, shutil, signal, socket, sqlite3, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from hub_cell import RustHub

GO, SRC = Path(sys.argv[2]), Path(sys.argv[3])

def post(port, req):
    body = json.dumps(req)
    with socket.create_connection(('::1', port), timeout=30) as s:
        s.sendall(f'POST /api/rpc HTTP/1.1\r\nHost: [::1]:{port}\r\nContent-Type: application/json\r\nX-Taskr-RPC: 1\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n{body}'.encode())
        data = b''
        while chunk := s.recv(65536):
            data += chunk
    return json.loads(data.decode().split('\r\n\r\n', 1)[-1])

with tempfile.TemporaryDirectory(prefix='rev-r2-swap-') as tmp:
    tmp = Path(tmp)
    install = tmp / 'install'; install.mkdir()
    exe = install / 'taskr'
    shutil.copy2(SRC, exe)
    place = tmp / 'cell'; place.mkdir()
    cell = RustHub(GO, exe, place)
    out = {}
    try:
        port = int(cell.url.rsplit(':', 1)[1])
        root = json.loads(cell.record(['new', 'swap-root', '--role', 'orchestrator']).stdout)['task_id']
        out['before'] = post(port, {'argv': ['note', 'before swap', '--as', str(root)], 'cwd': '/', 'request_key': 'swap-before-0001'})
        staged = install / '.taskr.new'
        shutil.copy2(SRC, staged)
        os.replace(staged, exe)  # install.sh: mv -f "$tmp/$ASSET" "$DIR/taskr"
        out['hub_proc_exe'] = os.readlink(f'/proc/{cell.hub.pid}/exe')
        out['after_status'] = post(port, {'argv': ['status'], 'cwd': '/', 'request_key': 'swap-status-0001'})
        out['after_note'] = post(port, {'argv': ['note', 'after swap', '--as', str(root)], 'cwd': '/', 'request_key': 'swap-after-0001'})
        with sqlite3.connect(cell.db) as db:
            out['after_note_row'] = db.execute("select state, exit, stdout from requests where key='swap-after-0001'").fetchone()
            out['after_note_events'] = db.execute("select count(*) from events where kind='note' and summary='after swap'").fetchone()[0]
        out['after_note_replay'] = post(port, {'argv': ['note', 'after swap', '--as', str(root)], 'cwd': '/', 'request_key': 'swap-after-0001'})
    finally:
        cell.hub.send_signal(signal.SIGTERM)
        try: cell.hub.wait(timeout=20)
        except subprocess.TimeoutExpired: cell.hub.kill()
        cell.close()

assert out['hub_proc_exe'].endswith(' (deleted)'), out
for name in ['before', 'after_status', 'after_note', 'after_note_replay']:
    assert out[name]['exit'] == 0, (name, out)
assert out['after_note'] == out['after_note_replay'], out
assert out['after_note_row'][:2] == ('done', 0), out
assert out['after_note_events'] == 1, out

print(json.dumps(out, indent=2))
