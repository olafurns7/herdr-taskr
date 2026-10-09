#!/usr/bin/env python3
"""Check Go/Rust HTTP deadlines using only synthetic hub fixtures."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools/contract'))
import golden
import argparse
from concurrent.futures import ThreadPoolExecutor
import http.client
import json
import select
import socket
import sqlite3
import tempfile
import time
from hub_cell import Cell, RustHub


def disconnected_write(cell):
    root = json.loads(cell.record(['new', 'detached-write-root', '--role', 'orchestrator']).stdout)['task_id']
    key = 'http-detached-write'
    request = json.dumps({'argv': ['--json', 'note', 'detached write', '--as', str(root)], 'cwd': '/', 'request_key': key})
    port = int(cell.url.rsplit(':', 1)[1])
    # Hold the write lock so disconnect happens before a claim or write can commit.
    with sqlite3.connect(cell.db) as blocker:
        blocker.execute('begin immediate')
        with socket.create_connection(('::1', port), timeout=5) as stream:
            stream.sendall(f'POST /api/rpc HTTP/1.1\r\nHost: [::1]:{port}\r\nContent-Type: application/json\r\nX-Taskr-RPC: 1\r\nContent-Length: {len(request)}\r\n\r\n{request}'.encode())
            assert not select.select([stream], [], [], 1)[0], 'write unexpectedly completed under lock'
        select.select([], [], [], .5)
        blocker.commit()
    deadline = time.monotonic() + 5
    while cell.count('select count(*) from requests where key=? and state=\'done\'', (key,)) != 1:
        assert time.monotonic() < deadline, 'stored write was canceled on disconnect'
        select.select([], [], [], .02)
    assert cell.count("select count(*) from events where kind='note' and summary='detached write'") == 1
    cell.compare(['--json', '--request-key', key, 'note', 'detached write', '--as', str(root)])
    assert cell.count("select count(*) from events where kind='note' and summary='detached write'") == 1
    return {'hub': type(cell).__name__, 'check': 'disconnected_stored_write_then_replay', 'pass': True}


def check(cell, kind):
    port = int(cell.url.rsplit(':', 1)[1])
    started = time.monotonic()
    with socket.create_connection(('::1', port), timeout=15) as stream:
        if kind == 'headers':
            stream.sendall(b'POST /api/rpc HTTP/1.1\r\nHost:')
            stream.recv(4096)
            elapsed = time.monotonic() - started
            assert 4.5 <= elapsed <= 7, (kind, elapsed)
        elif kind == 'body':
            stream.sendall(b'POST /api/rpc HTTP/1.1\r\nHost:')
            select.select([], [], [], 4)
            stream.sendall(f' [::1]:{port}\r\nContent-Type: application/json\r\nX-Taskr-RPC: 1\r\nContent-Length: 100\r\n\r\n{{'.encode())
            stream.recv(4096)
            elapsed = time.monotonic() - started
            assert 9.5 <= elapsed <= 12, (kind, elapsed)
        elif kind == 'oversized':
            stream.sendall(f'GET / HTTP/1.1\r\nHost: [::1]:{port}\r\nX-Large: '.encode() + b'a' * 32768 + b'\r\n\r\n')
            assert b'431' in stream.recv(4096)
            elapsed = time.monotonic() - started
        else:
            request = json.dumps({'argv': ['version'], 'cwd': '/', 'request_key': 'http-idle-check'})
            stream.sendall(f'POST /api/rpc HTTP/1.1\r\nHost: [::1]:{port}\r\nContent-Type: application/json\r\nX-Taskr-RPC: 1\r\nContent-Length: {len(request)}\r\n\r\n{request}'.encode())
            response = http.client.HTTPResponse(stream)
            response.begin()
            assert response.status == 200
            response.read()
            started = time.monotonic()
            assert not select.select([stream], [], [], 55)[0], 'idle connection closed early'
            stream.settimeout(10)
            assert stream.recv(1) == b'', 'idle connection did not close'
            elapsed = time.monotonic() - started
            assert 59 <= elapsed <= 63, (kind, elapsed)
    return {'hub': type(cell).__name__, 'check': kind, 'seconds': round(elapsed, 3), 'pass': True}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--go', required=True, type=Path)
    parser.add_argument('--rust', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    args = golden.parse(parser, __file__)
    with tempfile.TemporaryDirectory(prefix='taskr-hub-http-') as tmp:
        cells = []
        try:
            for name, cls in [('Go', Cell), ('Rust', RustHub)]:
                place = Path(tmp).resolve() / name
                place.mkdir()
                cells.append(cls(args.go.resolve(), args.rust.resolve(), place))
            for cell in cells:
                cell.defer_observations = True
                cell.pending_observations = []
            with ThreadPoolExecutor(max_workers=10) as pool:
                checks = [pool.submit(check, cell, kind) for cell in cells for kind in ('headers', 'body', 'oversized', 'idle')]
                checks.extend(pool.submit(disconnected_write, cell) for cell in cells)
                result = {'pass': len(checks), 'checks': [future.result() for future in checks]}
            for cell in cells:
                for observation in cell.pending_observations: golden.observe(*observation)
            args.out.write_text(json.dumps(result, indent=2) + '\n')
            print(json.dumps(result))
        finally:
            for cell in cells:
                cell.close()

    golden.finish()
