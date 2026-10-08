#!/usr/bin/env python3
"""P5a Rust hub SSE acceptance cell: synthetic ledger, HOME and tailnet only."""
import argparse
import http.client
import json
from pathlib import Path
import select
import sqlite3
import subprocess
import tempfile
import time
from hub_cell import RustHub

class Subscriber:
    def __init__(self, cell, url=None, headers=None, code=200):
        url = url or cell.url
        host, port = url.removeprefix('http://').rsplit(':', 1)
        self.connection = http.client.HTTPConnection(host.strip('[]'), int(port), timeout=2)
        self.connection.request('GET', '/api/events', headers={'X-Taskr-RPC': '1', **(headers or {})})
        self.response = self.connection.getresponse()
        assert self.response.status == code, (self.response.status, self.response.read())
        if code == 200:
            assert self.response.getheader('Content-Type') == 'text/event-stream'
            assert self.response.getheader('Cache-Control') == 'no-store'
    def event(self):
        lines = []
        while True:
            text = self.response.readline().decode().strip()
            assert text or lines, 'stream ended unexpectedly'
            if not text:
                if not any(s.startswith('data:') for s in lines):
                    lines = []
                    continue
                kind = next(s.removeprefix('event:').strip() for s in lines if s.startswith('event:'))
                event = json.loads(next(s.removeprefix('data:').strip() for s in lines if s.startswith('data:')))
                assert set(event) <= {'epoch', 'rev', 'seq', 'kinds'}, event
                assert event['epoch'] and event['rev'] >= 0 and event['seq'] > 0, event
                return kind, event
            lines.append(text)
    def close(self):
        self.response.close()
        self.connection.close()

def cli(binary, args, env):
    p = subprocess.run([str(binary), *args], env=env, capture_output=True, timeout=10)
    assert p.returncode == 0, (args, p.returncode, p.stdout, p.stderr)
    return p

def cpu(pid):
    # Linux stat fields 14 and 15, following the parenthesized comm.
    fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
    return int(fields[11]) + int(fields[12])

def run(binary, idle):
    results = {}
    with tempfile.TemporaryDirectory(prefix='taskr-events-') as tmp:
        cell = RustHub(binary, binary, tmp)
        subs = []
        processes = []
        try:
            local_url = cell.hub.stdout.readline().decode().strip()
            with sqlite3.connect(cell.db) as db:
                db.execute("insert into meta(key,value) values('dashboard_url',?)", (local_url,))
            root = json.loads(cli(binary, ['--json', 'new', 'events-root', '--role', 'orchestrator'], cell.client_env).stdout)['task_id']
            local_root = json.loads(cli(binary, ['--json', 'new', 'local-events-root', '--role', 'orchestrator'], cell.hub_env).stdout)['task_id']
            sub = Subscriber(cell)
            subs.append(sub)
            kind, initial = sub.event()
            assert kind == 'change'
            results['initial'] = initial
            for label, env in [('rpc', cell.client_env), ('local_cli', cell.hub_env)]:
                start = time.monotonic()
                cli(binary, ['note', f'{label} change', '--as', str(root if label == 'rpc' else local_root)], env)
                expected = cell.count('select max(id) from events')
                while True:
                    kind, event = sub.event()
                    if event['rev'] == expected: break
                elapsed = time.monotonic() - start
                assert event['rev'] > initial['rev'] and event['epoch'] == initial['epoch'], (label, event)
                assert elapsed < 1, (label, elapsed)
                results[label + '_seconds'] = elapsed
                initial = event
            # Host observations must notify even if no ledger event was inserted.
            with sqlite3.connect(cell.db) as db:
                before = db.execute('select max(id) from events').fetchone()[0]
            c = http.client.HTTPConnection('::1', int(cell.url.rsplit(':', 1)[1]), timeout=5)
            body = json.dumps({'argv': ['_host', 'observe', '--agents', '[]'], 'cwd': tmp, 'request_key': 'events-host-observe'})
            c.request('POST', '/api/rpc', body=body, headers={'Content-Type': 'application/json', 'X-Taskr-RPC': '1'})
            r = c.getresponse()
            reply = json.loads(r.read()); c.close()
            assert r.status == 200 and reply['exit'] == 0, reply
            _, host_event = sub.event()
            assert host_event['rev'] == before, host_event
            results['host_without_event'] = True
            # A committed batch advances the revision by more than one: reset.
            with sqlite3.connect(cell.db) as db:
                for n in range(3):
                    db.execute("insert into events(task_id,kind,summary,created_at) values(?,'note',?,'2026-10-08T00:00:00Z')", (root, f'gap-{n}'))
            while True:
                kind, gap = sub.event()
                if gap['rev'] == before + 3: break
            assert kind == 'reset' and gap['rev'] == before + 3, (kind, gap)
            results['revision_gap_reset'] = True
            sub.close(); subs.remove(sub)
            # Eventless new tasks commit without changing the ledger revision.
            for label, env in [('rpc', cell.client_env), ('local', cell.hub_env)]:
                s = Subscriber(cell)
                _, baseline = s.event()
                # Consume outstanding request bookkeeping until a quiet poll interval.
                s.response.fp.raw._sock.settimeout(0.4)
                try:
                    while True:
                        _, baseline = s.event()
                except TimeoutError:
                    pass
                s.close()
                s = Subscriber(cell)
                _, baseline = s.event()
                cli(binary, ['new', f'eventless-{label}', '--role', 'orchestrator'], env)
                _, changed = s.event()
                assert changed['rev'] == baseline['rev'] and changed['seq'] > baseline['seq'], (baseline, changed)
                s.close()
                results[label + '_eventless_new'] = True
            # Old epochs and missing revisions require an initial resnapshot.
            for last in ['previous-process:1', f"{gap['epoch']}:0"]:
                s = Subscriber(cell, headers={'Last-Event-ID': last})
                assert s.event()[0] == 'reset'
                s.close()
            results['reconnect_reset'] = True
            for headers, code in [({'Origin': 'http://evil.example'}, 403), ({'Origin': ''}, 403), ({'Sec-Fetch-Site': 'same-origin'}, 403), ({'Host': 'evil.example'}, 421), ({'X-Taskr-RPC': ''}, 400)]:
                s = Subscriber(cell, headers=headers, code=code); s.close()
            # Use IPv4 for the explicit local-host path, even in contract mode.
            for headers, code in [({}, 200), ({'Origin': 'http://evil.example'}, 403)]:
                s = Subscriber(cell, url=local_url, headers=headers, code=code)
                if code == 200:
                    assert s.event()[1]['epoch'] == gap['epoch']
                s.close()
            # Change the synthetic whois fixture to reject the connecting peer.
            ts = Path(tmp)/'bin/tailscale'
            original = ts.read_text()
            ts.write_text(original.replace('login=owner@example.com; tags=', 'login=other@example.com; tags='))
            s = Subscriber(cell, code=403); s.close(); ts.write_text(original)
            results['admission'] = True
            # Exercise the real helper, both remote and local discovery.
            for label, env in [('remote', cell.client_env), ('local', cell.hub_env)]:
                p = subprocess.Popen([str(binary), '_events', '--json'], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                processes.append(p)
                assert select.select([p.stdout], [], [], 5)[0], label
                data = json.loads(p.stdout.readline())
                assert data['epoch'] == gap['epoch'] and data['event'] in ['change', 'reset'], data
                previous = data
                cli(binary, ['note', f'{label} helper write', '--as', str(root if label == 'remote' else local_root)], env)
                expected = cell.count('select max(id) from events')
                while data['rev'] != expected:
                    assert select.select([p.stdout], [], [], 5)[0], label
                    data = json.loads(p.stdout.readline())
                assert data['seq'] > previous['seq'], (previous, data)
                p.terminate(); p.wait(timeout=3); processes.remove(p)
                results[label + '_helper'] = True
            # Remote client rejects a tagged hub through shared RPC verification.
            p = subprocess.run([str(binary), '_events', '--json'], env={**cell.client_env, 'NET_DENY_HUB': 'tag'}, capture_output=True, timeout=10)
            assert p.returncode == 5 and b'hub not verified' in p.stdout, (p.returncode, p.stdout)
            results['client_peer_verification'] = True
            # Local discovery never creates a missing DB or accepts a remote URL.
            absent = Path(tmp) / 'missing.db'
            p = subprocess.run([str(binary), '_events'], env={**cell.hub_env, 'TASKR_DB': str(absent)}, capture_output=True, timeout=5)
            assert p.returncode == 2 and not absent.exists(), (p.returncode, p.stdout)
            with sqlite3.connect(cell.db) as db:
                db.execute("update meta set value='http://192.0.2.1:1234' where key='dashboard_url'")
            p = subprocess.run([str(binary), '_events'], env=cell.hub_env, capture_output=True, timeout=5)
            assert p.returncode == 6, (p.returncode, p.stdout)
            with sqlite3.connect(cell.db) as db:
                db.execute("update meta set value=? where key='dashboard_url'", (local_url,))
            results['local_discovery_readonly_loopback_only'] = True
            # Wait for closed bodies to release their permits, by bounded retries.
            deadline = time.monotonic() + 3
            while True:
                c = http.client.HTTPConnection('::1', int(cell.url.rsplit(':', 1)[1]), timeout=2)
                c.request('GET', '/api/events', headers={'X-Taskr-RPC': '1'})
                response = c.getresponse()
                if response.status == 200:
                    response.close(); c.close(); break
                response.close(); c.close()
                assert time.monotonic() < deadline
            for _ in range(32):
                s = Subscriber(cell); s.event(); subs.append(s)
            s = Subscriber(cell, code=503); s.close()
            results['subscriber_cap_32'] = True
            for s in subs:
                s.close()
            subs.clear()
            # Idle CPU and keepalives through the real chunked HTTP stream.
            s = Subscriber(cell); s.event(); subs.append(s)
            ticks = cpu(cell.hub.pid)
            started = time.monotonic()
            keepalives = 0
            idle_socket = s.response.fp.raw._sock
            while time.monotonic() - started < idle:
                remaining = idle - (time.monotonic() - started)
                if not select.select([idle_socket], [], [], remaining)[0]: break
                idle_socket.settimeout(2)
                lines = []
                while True:
                    text = s.response.readline().decode().strip()
                    if not text: break
                    lines.append(text)
                if ': keepalive' in lines: keepalives += 1
            elapsed = time.monotonic() - started
            import os
            cpu_seconds = (cpu(cell.hub.pid) - ticks) / os.sysconf('SC_CLK_TCK')
            assert keepalives >= 1
            results['idle'] = {'wall_seconds': elapsed, 'cpu_seconds': cpu_seconds, 'one_core_percent': cpu_seconds / elapsed * 100, 'keepalives': keepalives}
            start = time.monotonic()
            cell.hub.terminate(); cell.hub.wait(timeout=3)
            assert cell.hub.returncode == 0, (cell.hub.returncode, Path(tmp, 'hub.log').read_text())
            elapsed = time.monotonic() - start
            assert elapsed < 2, elapsed
            results['shutdown_seconds'] = elapsed
            s.response.read()  # Consumes any final buffered frames and the clean chunk terminator.
        finally:
            for p in processes:
                p.terminate(); p.wait(timeout=3)
            for s in subs:
                s.close()
            cell.close()
    return results

if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--rust', type=Path, required=True)
    p.add_argument('--idle-seconds', type=float, default=60)
    p.add_argument('--out', type=Path)
    args = p.parse_args()
    result = run(args.rust.resolve(), args.idle_seconds)
    if args.out:
        args.out.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))
