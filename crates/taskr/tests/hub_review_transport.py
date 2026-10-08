#!/usr/bin/env python3
"""R2: WAL reads, response write timeout, bounded drain and wait reaping."""
import argparse
import http.client
import json
from pathlib import Path
import select
import signal
import socket
import sqlite3
import tempfile
import time
from hub_cell import RustHub


def request(port, argv, key):
    stream = socket.socket(socket.AF_INET6)
    stream.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
    stream.settimeout(10)
    stream.connect(('::1', port))
    body = json.dumps({'argv': argv, 'cwd': '/', 'request_key': key}).encode()
    stream.sendall(f'POST /api/rpc HTTP/1.1\r\nHost: [::1]:{port}\r\nContent-Type: application/json\r\nX-Taskr-RPC: 1\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n'.encode() + body)
    return stream


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--go', required=True, type=Path)
    parser.add_argument('--rust', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='taskr-hub-transport-review-') as tmp:
        cell = RustHub(args.go.resolve(), args.rust.resolve(), tmp)
        try:
            root = json.loads(cell.record(['new', 'transport-root', '--role', 'orchestrator']).stdout)['task_id']
            port = int(cell.url.rsplit(':', 1)[1])
            with sqlite3.connect(cell.db) as writer:
                writer.execute('begin immediate')
                started = time.monotonic()
                with request(port, ['--json', 'status'], 'review-read-under-lock') as stream:
                    response = http.client.HTTPResponse(stream)
                    response.begin()
                    reply = json.loads(response.read())
                read_seconds = time.monotonic() - started
                assert reply['exit'] == 0 and read_seconds < 2, (read_seconds, reply)
                writer.rollback()
                writer.execute("insert into events(task_id,kind,summary,created_at) values(?,'note',?,'2026-10-08T00:00:00.000Z')", (root, 'x' * (12 << 20)))
            with request(port, ['--json', 'log', str(root), '--limit', '0'], 'review-write-deadline') as stream:
                first = stream.recv(1)
                assert first, 'no response began'
                started = time.monotonic()
                select.select([], [], [], 16)
                raw = bytearray(first)
                while chunk := stream.recv(65536):
                    raw.extend(chunk)
                write_seconds = time.monotonic() - started
                body = raw.split(b'\r\n\r\n', 1)[1]
                try:
                    json.loads(body)
                except json.JSONDecodeError:
                    pass  # The deadline closed a response blocked on an unread socket.
                else:
                    raise AssertionError('slow reader received the whole response; no write deadline')
            waiting = request(port, ['--json', 'wait', '--as', str(root), '--for', 'done', '--timeout', '20000'], 'review-reap-wait')
            large = request(port, ['--json', 'log', str(root), '--limit', '0'], 'review-stop-slow-writer')
            try:
                deadline = time.monotonic() + 5
                while cell.count('select waiting_until is not null from tasks where id=?', (root,)) != 1:
                    assert time.monotonic() < deadline, 'wait marker never appeared'
                    select.select([], [], [], .02)
                assert large.recv(1), 'no response began'
                started = time.monotonic()
                cell.hub.send_signal(signal.SIGTERM)
                cell.hub.wait(timeout=5)
                stop_seconds = time.monotonic() - started
                assert stop_seconds < 4, stop_seconds
                assert cell.count('select waiting_until is not null from tasks where id=?', (root,)) == 0, 'wait child was not reaped after clearing its marker'
            finally:
                large.close()
                waiting.close()
            result = {'pass': 4, 'read_under_writer_lock_seconds': round(read_seconds, 3), 'write_deadline_observed_seconds': round(write_seconds, 3), 'stop_seconds': round(stop_seconds, 3), 'waiting_marker_cleared': True}
            args.out.write_text(json.dumps(result, indent=2) + '\n')
            print(json.dumps(result))
        finally:
            cell.close()
