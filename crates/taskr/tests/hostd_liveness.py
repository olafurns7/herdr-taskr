#!/usr/bin/env python3
"""Actual hub glance freshness after Go/Rust relay and stale transitions."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools/contract'))
import golden
import argparse
from datetime import datetime, timedelta, timezone
import json
import sqlite3
import tempfile
from daemon_cell import Herdr, eventually, start, stop
from net_cell import Cell
from hostd_cell import setup, agent, snapshot


def run(go, rust, out):
    evidence = []
    with tempfile.TemporaryDirectory(prefix='taskr-hostd-live-') as tmp:
        cell = Cell(rust, go, tmp)
        fake = Herdr(cell.client_home)
        cell.client_env.update(HERDR_SOCKET_PATH=str(fake.path), PATH=str(cell.client_home/'bin')+':'+cell.env['PATH'])
        relay = None
        try:
            _, _, launch = setup(cell, rust)
            (cell.client_home/'agents.json').write_text(json.dumps(agent(1)))
            def stale_hosts():
                view = cell.obj(rust, ['glance'])
                return sorted(row['host'] for row in view.get('attention', []) if row.get('kind') == 'host_stale')
            stale = (datetime.now(timezone.utc)-timedelta(seconds=31)).isoformat().replace('+00:00', 'Z')
            for index, binary in enumerate((go, rust)):
                with sqlite3.connect(cell.db) as db:
                    db.execute("insert into meta values('daemon_heartbeat:host-a',?) on conflict(key) do update set value=excluded.value", (stale,))
                assert stale_hosts() == ['host-a']
                response = cell.obj(binary, ['daemon', '--once'])
                if index == 0: golden.observe('Go relay once', response, 'Rust hub / Go relay client')
                assert stale_hosts() == []
                assert snapshot(cell, launch) == ('working', 1, 1)
                evidence.append({'relay': 'Go' if binary == go else 'Rust', 'stale_before': ['host-a'], 'stale_after': []})
            relay = start(rust, cell.client_env)
            eventually(lambda: snapshot(cell, launch) == ('working', 1, 1))
            # A current heartbeat RPC (no agent-state change) clears an expired
            # heartbeat marker through the same hub-read predicate as full.
            with sqlite3.connect(cell.db) as db:
                db.execute("update meta set value=? where key='daemon_heartbeat:host-a'", (stale,))
            assert stale_hosts() == ['host-a']
            eventually(lambda: stale_hosts() == [], timeout=12)
            evidence.append({'relay': 'Rust resident heartbeat', 'stale_before': ['host-a'], 'stale_after': []})
        finally:
            if relay is not None: stop(relay)
            fake.close(); cell.close()
    out.write_text(json.dumps({'passed': len(evidence), 'evidence': evidence}, indent=2)+'\n')
    print(json.dumps({'passed': len(evidence)}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    for name in ('go', 'rust', 'out'):
        parser.add_argument('--'+name, type=Path, required=True)
    args = golden.parse(parser, __file__)
    run(args.go.resolve(), args.rust.resolve(), args.out)

    golden.finish()
