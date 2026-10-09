#!/usr/bin/env python3
"""Run the entire synthetic golden corpus. No installed binary or Go toolchain discovery."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
CELLS = ('daemon_cell', 'daemon_hub_cell', 'daemon_limits', 'daemon_relay_cell',
         'daemon_restart_cell', 'hostd_cell', 'hostd_legacy', 'hostd_liveness',
         'hostd_wire', 'hub_cell', 'hub_child_context', 'hub_http_limits',
         'hub_review_transport', 'net_cell')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument('--record', type=Path)
    modes.add_argument('--expect', type=Path)
    parser.add_argument('--production-go', type=Path, help='production Go binary for sanitized restart/local-child probes')
    parser.add_argument('--rust', type=Path, required=True)
    args = parser.parse_args()
    subprocess.run([sys.executable, str(ROOT / 'tools/contract/selfcheck.py')], cwd=ROOT, check=True)
    mode = ['--record', str(args.record.resolve())] if args.record else ['--expect', str(args.expect.resolve())]
    with tempfile.TemporaryDirectory(prefix='taskr-golden-results-') as tmp:
        def check(name):
            if sys.platform != 'linux' and name in ('daemon_cell', 'daemon_restart_cell', 'daemon_limits'):
                print(f'SKIP {name}: Linux /proc process proof', flush=True)
                return 0
            script = ROOT / ('tools/contract/run.py' if name == 'run' else f'crates/taskr/tests/{name}.py')
            selected = ['--record', str(args.production_go.resolve())] if args.record and args.production_go and name in ('daemon_restart_cell', 'hub_child_context') else mode
            command = [sys.executable, str(script), *selected, '--rust', str(args.rust.resolve()),
                       '--out', str(Path(tmp) / (name + '.json'))]
            if name == 'run':
                command.append('--catchup-fixtures')
            if name == 'hub_child_context':
                command += ['--contract', str(args.rust.resolve())]
            if name == 'hub_cell':
                command.append('--net-fixture')
            result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
            print(f'{name}: exit={result.returncode}', flush=True)
            if result.returncode:
                print(result.stdout + result.stderr, flush=True)
            return result.returncode

        serial = ('daemon_restart_cell', 'hub_cell', 'net_cell', 'hostd_cell')
        with ThreadPoolExecutor(max_workers=4) as pool:
            codes = list(pool.map(check, ('run', *(name for name in CELLS if name not in serial))))
        codes.extend(check(name) for name in serial)
        return int(any(codes))


if __name__ == '__main__':
    raise SystemExit(main())
