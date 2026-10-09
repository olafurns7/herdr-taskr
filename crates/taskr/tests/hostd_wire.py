#!/usr/bin/env python3
"""Admitted _host wire boundaries and snapshot generation recovery."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools/contract'))
import golden
import argparse
import concurrent.futures
import http.client
import json
import sqlite3
import tempfile
from net_cell import Cell
from events_cell import Subscriber
from hostd_cell import setup, agent, snapshot, restart_hub


def run(go, rust, out):
    checks = []
    with tempfile.TemporaryDirectory(prefix='taskr-hostd-wire-') as tmp:
        cell = Cell(rust, go, tmp)
        try:
            top, _, launch = setup(cell, rust)
            def rpc(kind, agents=None, removed=None, epoch=None, base=None, code=0):
                argv = ['--json', '_host', kind]
                for flag, value in [('agents', agents), ('removed', removed), ('epoch', epoch), ('base', base)]:
                    if value is not None:
                        argv += ['--'+flag, json.dumps(value) if isinstance(value, (list, dict)) else str(value)]
                connection = http.client.HTTPConnection('::1', int(cell.url.rsplit(':', 1)[1]), timeout=15)
                connection.request('POST', '/api/rpc', json.dumps({'argv': argv, 'cwd': str(cell.tmp), 'request_key': cell.key()}), {'X-Taskr-RPC': '1', 'Content-Type': 'application/json'})
                response = connection.getresponse()
                assert response.status == 200, response.status
                rep = json.loads(response.read())
                connection.close()
                if code is not None:
                    assert rep['exit'] == code, rep
                return rep, json.loads(rep['stdout'].splitlines()[-1])
            def meta(key):
                with sqlite3.connect(cell.db) as db:
                    row = db.execute('select value from meta where key=?', (key,)).fetchone()
                    return row[0] if row else None
            _, reply = rpc('observe', agent(1))
            epoch = reply['hostd']['epoch']; base = reply['hostd']['generation']
            assert reply['hostd']['stale_ms'] == 30000 and snapshot(cell, launch) == ('working', 1, 1)
            subscription = Subscriber(cell)
            try: assert subscription.event()[1]['epoch'] == epoch
            finally: subscription.close()
            checks.append('legacy full accepted and advertises the actual SSE process epoch')
            before = meta('hostd_snapshot:host-a')
            _, heartbeat = rpc('heartbeat', epoch=epoch, base=base)
            assert heartbeat['observed'] == 0 and heartbeat['hostd']['generation'] == base
            assert meta('hostd_snapshot:host-a') == before
            checks.append('heartbeat does not rewrite snapshot or apply observations')
            stored = json.loads(meta('hostd_snapshot:host-a'))
            assert next(row for row in stored['inputs'] if row[0] == top)[5:] == [1, 'working'], stored
            cell.want(rust, ['adopt', str(top), '--pane', 'wRoot:p0'])
            rpc('heartbeat', epoch=epoch, base=base, code=6)
            _, reply = rpc('observe', agent(1))
            base = reply['hostd']['generation']
            assert cell.count('select lead_status from tasks where id=?', (top,)) == 'working'
            rpc('heartbeat', epoch=epoch, base=base)
            checks.append('same-pane adopt invalidates lead inputs; post-observe inputs stabilize heartbeat')
            closed = cell.obj(rust, ['new', 'archived', '--role', 'orchestrator'])['task_id']
            cell.obj(rust, ['close', str(closed)])
            _, reply = rpc('observe', agent(1))
            base = reply['hostd']['generation']
            assert closed not in [row[0] for row in json.loads(meta('hostd_snapshot:host-a'))['inputs']]
            with sqlite3.connect(cell.db) as db:
                db.execute("update tasks set lead_status='blocked',lead_present=0 where id=?", (closed,))
            rpc('heartbeat', epoch=epoch, base=base)
            checks.append('closed tasks excluded from inputs and cannot invalidate a heartbeat')
            for bad_epoch, bad_base in [(epoch, base+1), ('old-epoch', base)]:
                before = meta('daemon_heartbeat:host-a')
                rpc('heartbeat', epoch=bad_epoch, base=bad_base, code=6)
                assert meta('daemon_heartbeat:host-a') == before
            rpc('delta', agent(2)[:1], epoch=epoch, base=base+1, code=6)
            assert snapshot(cell, launch) == ('working', 1, 1)
            checks.append('wrong epoch/base rejected without refreshing heartbeat or observation')
            saved_snapshot = meta('hostd_snapshot:host-a')
            for bad in [None, {}, [3], [{'pane_id': 'wLane:p1', 'agent_status': 7}], [{'pane_id': 'wLane:p1'}, agent(2)[0]]]:
                # Null is a literal JSON payload here, not an omitted flag.
                rpc('delta', 'null' if bad is None else bad, epoch=epoch, base=base, code=2)
                assert meta('hostd_snapshot:host-a') == saved_snapshot
                assert snapshot(cell, launch) == ('working', 1, 1)
            rpc('delta', [], removed={'forged': True}, epoch=epoch, base=base, code=2)
            checks.append('malformed and shadowed-invalid delta entries and removals rejected')
            # Concurrent RPC children cannot both apply the same base generation.
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                futures = [pool.submit(rpc, 'delta', agent(seq)[:1], epoch=epoch, base=base, code=None) for seq in (2, 3)]
                replies = [f.result() for f in futures]
            assert sorted(rep['exit'] for rep, _ in replies) == [0, 6], replies
            base += 1
            checks.append('concurrent delta base has one winner and one resync rejection')
            _, reply = rpc('delta', [], removed=['wLane:p1'], epoch=epoch, base=base)
            assert snapshot(cell, launch)[2] == 0
            base = reply['hostd']['generation']
            _, reply = rpc('delta', agent(4)[:1], epoch=epoch, base=base)
            assert snapshot(cell, launch) == ('working', 4, 1)
            checks.append('pane deletion and reappearance preserve scoped CAS')
            class Upstream:
                upstream = cell.url
            upstream = Upstream()
            restart_hub(cell, rust, upstream)
            cell.url = upstream.upstream
            rpc('heartbeat', epoch=epoch, base=reply['hostd']['generation'], code=6)
            _, reply = rpc('observe', agent(5))
            assert reply['hostd']['epoch'] != epoch and snapshot(cell, launch) == ('working', 5, 1)
            checks.append('epoch change requires full resync and preserves old-client full compatibility')
            with sqlite3.connect(cell.db) as db:
                db.execute("delete from meta where key='hostd_snapshot:host-a'")
            rpc('heartbeat', epoch=reply['hostd']['epoch'], base=reply['hostd']['generation'], code=6)
            checks.append('missing snapshot fails closed')
        finally:
            cell.close()
    out.write_text(json.dumps({'passed': len(checks), 'checks': checks}, indent=2)+'\n')
    print(json.dumps({'passed': len(checks)}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    for name in ('go', 'rust', 'out'):
        parser.add_argument('--'+name, type=Path, required=True)
    args = golden.parse(parser, __file__)
    run(args.go.resolve(), args.rust.resolve(), args.out)

    golden.finish()
