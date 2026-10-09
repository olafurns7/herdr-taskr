#!/usr/bin/env python3
"""Changed-state uplink acceptance. Synthetic hubs, socket, HOME and tailnet only."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools/contract'))
import golden
import argparse
from datetime import datetime, timezone
import http.client
import json
import select
import socket
import socketserver
import sqlite3
from statistics import median
import subprocess
import tempfile
import threading
import time
from daemon_cell import Herdr, eventually, start, stop
from net_cell import Cell


class Tap:
    def __init__(self, upstream):
        self.upstream = upstream
        self.calls = []
        self.drop = False
        owner = self

        class Server(socketserver.ThreadingTCPServer):
            address_family = socket.AF_INET6
            daemon_threads = True
            allow_reuse_address = True

        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                first = self.rfile.readline()
                if not first:
                    return
                headers = {}
                while True:
                    line = self.rfile.readline()
                    if line == b'\r\n':
                        break
                    k, v = line.decode().split(':', 1)
                    headers[k.lower()] = v.strip()
                body = self.rfile.read(int(headers.get('content-length', 0)))
                request = json.loads(body)
                owner.calls.append({'at': time.monotonic(), 'argv': request['argv']})
                port = int(owner.upstream.rsplit(':', 1)[1])
                connection = http.client.HTTPConnection('::1', port, timeout=20)
                try:
                    connection.request('POST', '/api/rpc', body, {'Host': f'[::1]:{port}', 'Content-Type': 'application/json', 'X-Taskr-RPC': '1'})
                    response = connection.getresponse()
                    data = response.read()
                    if owner.drop:
                        owner.drop = False
                        return
                    self.wfile.write(f'HTTP/1.1 {response.status} OK\r\nContent-Type: application/json\r\nContent-Length: {len(data)}\r\nConnection: close\r\n\r\n'.encode()+data)
                finally:
                    connection.close()

        self.server = Server(('::1', 0), Handler)
        self.url = 'http://[::1]:'+str(self.server.server_address[1])
        if golden.session: golden.session.ports.add(self.server.server_address[1])
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def hosts(self, after=0):
        return [c for c in self.calls[after:] if '_host' in c['argv']]


def token_fake(fake):
    tokens = fake.home/'fixture-tokens.json'
    empty = {'panes': {'wLane:p1': {}}, 'workspaces': {'wLane': {}, 'wRoot': {}}}
    tokens.write_text(json.dumps(empty))
    (fake.home/'bin/herdr').write_text("#!/usr/bin/python3\nimport json, os, sys\nfrom pathlib import Path\nhome = Path(os.environ['HOME']); args = sys.argv[1:]\nwith (home/'calls').open('a') as output: output.write(json.dumps(args)+'\\n')\npath = home/'fixture-tokens.json'; state = json.loads(path.read_text())\nif args[:2] == ['agent','list']: result = {'agents':json.loads((home/'agents.json').read_text())}\nelif args[:2] == ['workspace','list']: result = {'workspaces':[{'workspace_id':key,'tokens':value} for key,value in state['workspaces'].items()]}\nelif args[:2] == ['pane','list']: result = {'panes':[{'pane_id':key,'tokens':value} for key,value in state['panes'].items()]}\nelif len(args)>2 and args[1] == 'report-metadata':\n    target = state['panes' if args[0]=='pane' else 'workspaces'].setdefault(args[2],{})\n    for i,arg in enumerate(args):\n        if arg == '--token':\n            key,value = args[i+1].split('=',1); target[key] = value\n        elif arg == '--clear-token': target.pop(args[i+1],None)\n    pending = path.with_suffix('.tmp'); pending.write_text(json.dumps(state)); pending.replace(path); result = {}\nelse: result = {}\nprint(json.dumps({'result':result}))\n")
    return tokens, empty


def status(cell, rust):
    value = cell.obj(rust, ['daemon', '--status', '--uplink'])
    assert set(value['uplink']) == {'full', 'delta', 'heartbeat', 'skipped_unchanged'}, value
    return value['uplink']


def fresh(cell):
    with sqlite3.connect(cell.db) as db:
        value = db.execute("select value from meta where key='daemon_heartbeat:host-a'").fetchone()
    assert value, 'missing client heartbeat'
    age = (datetime.now(timezone.utc)-datetime.fromisoformat(value[0].replace('Z', '+00:00'))).total_seconds()
    assert 0 <= age < 30, age
    return age


def restart_hub(cell, rust, tap):
    stop(cell.hub)
    cell.hub = subprocess.Popen([str(rust), 'daemon', '--stay'], env=cell.hub_env, stdout=cell.log, stderr=cell.log)
    def bound():
        try:
            with sqlite3.connect(cell.db) as db:
                row = db.execute("select value from meta where key='dashboard_url'").fetchone()
            if not row:
                return False
            port = row[0].split(':')[-1].strip('/')
            with socket.create_connection(('::1', int(port)), timeout=.2):
                tap.upstream = 'http://[::1]:'+port
                return True
        except (OSError, sqlite3.Error):
            return False
    eventually(bound)


def setup(cell, rust):
    top = cell.obj(rust, ['new', 'campaign', '--role', 'orchestrator', '--pane', 'wRoot:p0', '--workspace', 'wRoot'])['task_id']
    worker = cell.obj(rust, ['new', 'lane', '--role', 'implementer', '--parent', str(top), '--pane', 'wLane:p1', '--workspace', 'wLane'])['task_id']
    launch = cell.obj(rust, ['launch', str(worker), '--provider', 'fixture', '--model', 'fixture', '--effort', 'medium'])['launch_id']
    return top, worker, launch


def agent(seq, state='working'):
    return [{'name': 'lane', 'pane_id': 'wLane:p1', 'agent_status': state, 'state_change_seq': seq}, {'name': 'campaign', 'pane_id': 'wRoot:p0', 'agent_status': 'working', 'state_change_seq': 1}]


def snapshot(cell, launch):
    with sqlite3.connect(cell.db) as db:
        return db.execute('select observed_status,observed_seq,present from launches where id=?', (launch,)).fetchone()


def acknowledged(cell):
    with sqlite3.connect(cell.db) as db:
        row = db.execute("select value from meta where key='hostd_snapshot:host-a'").fetchone()
    return json.loads(row[0])


def run(go, rust, out, idle_seconds):
    evidence = {}
    with tempfile.TemporaryDirectory(prefix='taskr-hostd-') as tmp:
        cell = Cell(rust, go, tmp)
        fake = Herdr(cell.client_home)
        cell.client_env.update(HERDR_SOCKET_PATH=str(fake.path), PATH=str(cell.client_home/'bin')+':'+cell.env['PATH'])
        fixture_tokens, empty_tokens = token_fake(fake)
        tap = Tap(cell.url)
        cell.server_file.write_text(tap.url+'\n')
        relay = None
        try:
            top, worker, launch = setup(cell, rust)
            (cell.client_home/'agents.json').write_text(json.dumps(agent(1)))
            relay = start(rust, cell.client_env)
            eventually(lambda: snapshot(cell, launch) == ('working', 1, 1))
            eventually(lambda: len(fake.requests) >= 2)
            # Let the acknowledgement of the pane-set refresh settle before the idle window.
            eventually(lambda: status(cell, rust)['skipped_unchanged'] >= 1)
            before = status(cell, rust)
            position = len(tap.calls)
            lists_before = (cell.client_home/'calls').read_text().count('["agent", "list"]')
            began = time.monotonic()
            max_age = 0
            while time.monotonic()-began < idle_seconds:
                max_age = max(max_age, fresh(cell))
                select.select([], [], [], min(.2, max(0, idle_seconds-(time.monotonic()-began))))
            after = status(cell, rust)
            calls = tap.hosts(position)
            assert calls and all(c['argv'][2] == 'heartbeat' for c in calls), calls
            assert after['full'] == before['full'] and after['delta'] == before['delta'], (before, after)
            assert after['heartbeat']-before['heartbeat'] == len(calls), (after, before, calls)
            agent_lists = (cell.client_home/'calls').read_text().count('["agent", "list"]') - lists_before
            assert 0 < agent_lists <= len(calls), (agent_lists, calls)
            print('Rust idle window complete', flush=True)
            evidence['idle'] = {'seconds': time.monotonic()-began, 'before': before, 'after': after, 'host_requests': len(calls), 'max_heartbeat_age_seconds': max_age, 'agent_lists': agent_lists}
            out.with_suffix('.partial.json').write_text(json.dumps(evidence, indent=2)+'\n')
            print(json.dumps({'idle': evidence['idle']}), flush=True)
            agents = agent(1)
            agents[1].update(agent_status='blocked', state_change_seq=2)
            (cell.client_home/'agents.json').write_text(json.dumps(agents))
            assert fake.wake('wRoot:p0') == 0
            began = time.monotonic()
            eventually(lambda: cell.count('select lead_status from tasks where id=?', (top,)) == 'blocked', timeout=12)
            assert 'lead_blocked' in [r['kind'] for r in cell.obj(rust, ['glance'])['attention']]
            evidence['unsubscribed_lead_blocked'] = {'seconds': time.monotonic()-began, 'status': 'blocked', 'lead_blocked': True}
            agents[0].update(agent_status='idle', state_change_seq=2)
            (cell.client_home/'agents.json').write_text(json.dumps(agents))
            began = time.monotonic()
            eventually(lambda: snapshot(cell, launch) == ('idle', 2, 1), timeout=12)
            evidence['lost_lane_event'] = {'seconds': time.monotonic()-began, 'status': 'idle', 'event_sent': False}
            began = time.monotonic()
            agents[0].update(agent_status='blocked', state_change_seq=3)
            (cell.client_home/'agents.json').write_text(json.dumps(agents))
            fake.wake()
            eventually(lambda: snapshot(cell, launch) == ('blocked', 3, 1), timeout=1)
            latency = time.monotonic()-began
            assert latency < 1, latency
            delta = next(c for c in reversed(tap.hosts()) if c['argv'][2] == 'delta')
            payload = json.loads(delta['argv'][delta['argv'].index('--agents')+1])
            assert len(payload) == 1 and payload[0]['pane_id'] == 'wLane:p1', delta
            evidence['pane_change_seconds'] = latency
            # An unchanged event must not produce another full or delta.
            sent = len(tap.hosts())
            fake.wake()
            eventually(lambda: status(cell, rust)['skipped_unchanged'] > after['skipped_unchanged'])
            assert all(c['argv'][2] == 'heartbeat' for c in tap.hosts()[sent:])
            # A removed pane travels in --removed and updates the same missing CAS as Go.
            (cell.client_home/'agents.json').write_text(json.dumps(agent(3)[1:]))
            fake.wake()
            eventually(lambda: snapshot(cell, launch)[2] == 0, timeout=1)
            assert json.loads(tap.hosts()[-1]['argv'][tap.hosts()[-1]['argv'].index('--removed')+1]) == ['wLane:p1']
            (cell.client_home/'agents.json').write_text(json.dumps(agent(4)))
            fake.wake()
            eventually(lambda: snapshot(cell, launch) == ('working', 4, 1), timeout=12)
            # A hub restart changes the epoch even when contract time is frozen.
            before = status(cell, rust)
            old_epoch = acknowledged(cell)['epoch']
            restart_hub(cell, rust, tap)
            eventually(lambda: status(cell, rust)['full'] > before['full'] and acknowledged(cell)['epoch'] != old_epoch, timeout=15)
            assert snapshot(cell, launch) == ('working', 4, 1)
            evidence['restart'] = {'before': before, 'after': status(cell, rust), 'fresh_age': fresh(cell)}
            # A lost successful delta reply causes a fresh full snapshot, never a reused base.
            before = status(cell, rust)
            generation = acknowledged(cell)['generation']
            tap.drop = True
            (cell.client_home/'agents.json').write_text(json.dumps(agent(5)))
            fake.wake()
            eventually(lambda: status(cell, rust)['full'] > before['full'] and acknowledged(cell)['generation'] >= generation+2, timeout=15)
            assert snapshot(cell, launch) == ('working', 5, 1)
            evidence['lost_reply_resync'] = status(cell, rust)
            # Herdr reconnect similarly invalidates the acknowledged snapshot.
            before = status(cell, rust)
            generation = acknowledged(cell)['generation']
            for connection in list(fake.streams):
                try: connection.shutdown(socket.SHUT_RDWR)
                except OSError: pass
            eventually(lambda: status(cell, rust)['full'] > before['full'] and acknowledged(cell)['generation'] > generation, timeout=8)
            evidence['herdr_reconnect_resync'] = status(cell, rust)
            before = status(cell, rust)
            generation = acknowledged(cell)['generation']
            for connection in list(fake.streams):
                try: connection.sendall(b'{"error":{"code":"fixture","message":"synthetic stream error"}}\n')
                except OSError: pass
            eventually(lambda: status(cell, rust)['full'] > before['full'] and acknowledged(cell)['generation'] > generation, timeout=8)
            evidence['herdr_error_resync'] = status(cell, rust)
            before = status(cell, rust)
            generation = acknowledged(cell)['generation']
            for connection in list(fake.streams):
                try: connection.sendall(b'{malformed\n')
                except OSError: pass
            eventually(lambda: 'malformed subscription message' in (cell.client_home/'.local/state/taskr/daemon.log').read_text(), timeout=8)
            eventually(lambda: status(cell, rust)['full'] > before['full'] and acknowledged(cell)['generation'] > generation, timeout=8)
            evidence['malformed_stream_resync'] = status(cell, rust)
            print('Restart, lost reply, Herdr reconnect and error recovery complete', flush=True)
            # Hub launch inputs changed, but Herdr's listing did not. The next
            # heartbeat requests a full application for the new launch.
            before = status(cell, rust)
            launch = cell.obj(rust, ['launch', str(worker), '--provider', 'fixture', '--model', 'fixture', '--effort', 'medium'])['launch_id']
            eventually(lambda: snapshot(cell, launch) == ('working', 5, 1) and status(cell, rust)['full'] > before['full'], timeout=12)
            evidence['unchanged_pane_new_launch_resync'] = status(cell, rust)
            out.with_suffix('.partial.json').write_text(json.dumps(evidence, indent=2)+'\n')
            # An idle heartbeat also reconciles owner-token inputs from the hub.
            ask = cell.obj(rust, ['ask', 'heartbeat owner question', '--owner', '--blocking'], env={'TASKR_TASK': str(worker), 'TASKR_LAUNCH': str(launch)})['ask_id']
            eventually(lambda: json.loads(fixture_tokens.read_text())['panes']['wLane:p1'].get('taskr_owner_ask') == '1', timeout=12)
            cell.obj(rust, ['answer', str(ask), 'synthetic answer'], env={'TASKR_TASK': str(top)})
            eventually(lambda: 'taskr_owner_ask' not in json.loads(fixture_tokens.read_text())['panes']['wLane:p1'], timeout=12)
            evidence['heartbeat_owner_token_reconciliation'] = True
            # Owner-ask inputs and token commands match Go's full relay on the same script.
            stop(relay); relay = None
            results = []
            for binary in (go, rust):
                (cell.client_home/'calls').write_text('')
                fixture_tokens.write_text(json.dumps(empty_tokens))
                ask = cell.obj(rust, ['ask', 'synthetic owner question', '--owner', '--blocking'], env={'TASKR_TASK': str(worker), 'TASKR_LAUNCH': str(launch)})['ask_id']
                cell.obj(binary, ['daemon', '--once'])
                rows = [json.loads(line) for line in (cell.client_home/'calls').read_text().splitlines()]
                results.append([a for a in rows if a[:2] in (['pane', 'report-metadata'], ['workspace', 'report-metadata'], ['notification', 'show'])])
                assert any('taskr_owner_ask=1' in a for a in results[-1]), results[-1]
                assert any(a[:2] == ['notification', 'show'] for a in results[-1]), results[-1]
                assert fresh(cell) < 30
                cell.obj(rust, ['answer', str(ask), 'synthetic answer'], env={'TASKR_TASK': str(top)})
            golden.observe('Go owner token commands', results[0])
            assert results[0] == results[1], results
            evidence['go_token_and_liveness_parity'] = results
            print('Owner tokens and Go command parity complete', flush=True)
            out.with_suffix('.partial.json').write_text(json.dumps(evidence, indent=2)+'\n')
            help_text = cell.want(rust, ['daemon', '--uplink', '--help']).stdout.decode()
            assert 'skipped-unchanged' in help_text and '--status' in help_text, help_text
            assert cell.want(rust, ['daemon', '-uplink', '--help']).stdout.decode() == help_text
            cell.want(rust, ['daemon', '--uplink'], code=2)
            # Extension defaults to JSON without requiring --json.
            assert json.loads(cell.want(rust, ['daemon', '--status', '--uplink']).stdout)['uplink']
        finally:
            if relay is not None: stop(relay)
            fake.close(); tap.close(); cell.close()

    with tempfile.TemporaryDirectory(prefix='taskr-hostd-go-') as tmp:
        cell = Cell(go, rust, tmp, legacy=True)
        fake = Herdr(cell.client_home)
        cell.client_env.update(HERDR_SOCKET_PATH=str(fake.path), PATH=str(cell.client_home/'bin')+':'+cell.env['PATH'])
        tap = Tap(cell.url); cell.server_file.write_text(tap.url+'\n')
        relay = None
        try:
            setup(cell, rust)
            (cell.client_home/'agents.json').write_text(json.dumps(agent(1)))
            relay = start(rust, cell.client_env)
            eventually(lambda: len(tap.hosts()) >= 5, timeout=18)
            stop(relay); relay = None
            calls = tap.hosts()
            assert all(c['argv'][2] == 'observe' for c in calls), calls
            # Delayed arrivals compress fixed ticks under load; use the median after startup and subscription acknowledgement.
            steady = calls[2:]
            gaps = [b['at']-a['at'] for a, b in zip(steady, steady[1:])]
            assert len(gaps) >= 2 and 4.0 <= median(gaps) <= 6.5, gaps
            assert all('--epoch' not in c['argv'] and '--base' not in c['argv'] for c in calls)
            counters = status(cell, rust)
            assert counters['delta'] == counters['heartbeat'] == 0 and counters['full'] == len(calls), (counters, calls)
            evidence['legacy_go_hub'] = {'gaps_seconds': gaps, 'all_observe_seconds': [c['at']-calls[0]['at'] for c in calls], 'counters': counters}
        finally:
            if relay is not None: stop(relay)
            fake.close(); tap.close(); cell.close()
    out.write_text(json.dumps(evidence, indent=2)+'\n')
    print(json.dumps({'pass': True, 'idle_seconds': evidence['idle']['seconds'], 'change_seconds': latency, 'legacy_gaps': gaps}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--go', type=Path, required=True)
    parser.add_argument('--rust', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--idle-seconds', type=float, default=120)
    args = golden.parse(parser, __file__)
    run(args.go.resolve(), args.rust.resolve(), args.out, args.idle_seconds)

    golden.finish()
