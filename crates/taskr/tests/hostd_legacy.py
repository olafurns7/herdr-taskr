#!/usr/bin/env python3
"""Go-compatible fixed cadence survives redundant Herdr notifications."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools/contract'))
import golden
import argparse
import json
import select
import tempfile
import time
from daemon_cell import Herdr, eventually, start, stop
from hostd_cell import Tap, setup, agent, status
from net_cell import Cell


def run(go, rust, out):
    with tempfile.TemporaryDirectory(prefix='taskr-hostd-cadence-') as tmp:
        cell = Cell(go, rust, tmp, legacy=True)
        fake = tap = relay = None
        try:
            fake = Herdr(cell.client_home)
            cell.client_env.update(HERDR_SOCKET_PATH=str(fake.path), PATH=str(cell.client_home/'bin')+':'+cell.env['PATH'])
            tap = Tap(cell.url); cell.server_file.write_text(tap.url+'\n')
            setup(cell, rust)
            (cell.client_home/'agents.json').write_text(json.dumps(agent(1)))
            relay = start(rust, cell.client_env)
            eventually(lambda: status(cell, rust)['full'] >= 2)
            position = len(tap.calls); began = time.monotonic(); events = []
            for delay in (2, 4, 6, 8):
                remaining = began+delay-time.monotonic()
                if remaining > 0: select.select([], [], [], remaining)
                events.append(time.monotonic())
                fake.wake()
            remaining = began+11-time.monotonic()
            if remaining > 0: select.select([], [], [], remaining)
            stop(relay); relay = None
            calls = tap.hosts(position)
            assert calls and all(c['argv'][2] == 'observe' for c in calls), calls
            periodic = [c['at']-began for c in calls if all(abs(c['at']-at)>1 for at in events)]
            assert periodic, {'calls': calls, 'events': events, 'reason': 'Herdr events postponed the fixed 5s tick'}
            counters = status(cell, rust)
            assert counters['full'] == len(tap.hosts()) and counters['heartbeat'] == counters['delta'] == 0
            result = {'passed': 1, 'event_seconds': [at-began for at in events], 'observe_seconds': [c['at']-began for c in calls], 'independent_tick_seconds': periodic, 'counters': counters}
        finally:
            if relay is not None: stop(relay)
            if fake is not None: fake.close()
            if tap is not None: tap.close()
            cell.close()
    out.write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    for name in ('go', 'rust', 'out'):
        parser.add_argument('--'+name, type=Path, required=True)
    args = golden.parse(parser, __file__)
    run(args.go.resolve(), args.rust.resolve(), args.out)

    golden.finish()
