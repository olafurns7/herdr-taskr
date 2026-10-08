#!/usr/bin/env python3
"""Synthetic R3 drill. Default binaries prove local handoff; contract binaries prove RPC.

tools/handoff-drill.py --rust dist-rust/taskr-rust-x86_64-unknown-linux-musl \
    --rust-contract /path/to/contract/taskr --out /tmp/handoff.json
Go v0.16.1 oracles are built from this tree unless --go/--go-contract are supplied.
No host services, installed binaries, or live state are used. Linux is required
for daemon --restart. Default-feature RPC is first exercised at the real swap,
over the real tailnet.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import traceback

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'crates/taskr/tests'))
from daemon_cell import Herdr, env, eventually, invoke, record, start, stop
from net_cell import Cell, InterruptProxy

spec = importlib.util.spec_from_file_location('contract_fixtures', ROOT / 'tools/contract/run.py')
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)

STEPS = [
    'default: copied busy fixture, Go local write and daemon',
    'default: mv over running Go, Rust restart and lock/pid handover',
    'default: Rust reads Go rows, writes, Go migrates current ledger',
    'default: mv over running Rust, Go restart, Rust rows readable',
    'RPC contract: Go hub/client relay, stored reply and in-flight wait',
    'RPC contract: hub down, Go spool queued, swap to Rust',
    'RPC contract: Rust relay drains Go spool; wait replay and dedupe',
    'RPC contract: Rust spool queued, rollback on current ledger to Go',
    'RPC contract: Go relay drains Rust spool; schema and dedupe preserved',
]


def install(source, target):
    """install.sh's copy-to-staging, chmod, mv -f, on the same filesystem."""
    staged = target.with_name('taskr.next')
    shutil.copy2(source, staged)
    staged.chmod(0o755)
    os.replace(staged, target)


def scalar(path, sql, args=()):
    with sqlite3.connect(path, timeout=5) as db:
        return db.execute(sql, args).fetchone()[0]


def schema(path):
    with sqlite3.connect(path) as db:
        assert db.execute('pragma integrity_check').fetchall() == [('ok',)]
        assert db.execute('pragma foreign_key_check').fetchall() == []
        return db.execute('select type,name,tbl_name,sql from sqlite_master order by type,name').fetchall()


def lock_held(path):
    import fcntl
    with path.open('a+b') as f:
        try:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        return False


def stop_detached(pid, lock):
    # Only pids returned by a scratch daemon restart are accepted here.
    if lock_held(lock):
        assert int(lock.read_text()) == pid, (pid, lock.read_text())
        os.kill(pid, signal.SIGTERM)
        eventually(lambda: not lock_held(lock))
    assert lock.read_bytes() == b''


def local(go, rust, place, busy, passed):
    home = place / 'home'; home.mkdir(parents=True)
    state = home / '.local/state/taskr'; state.mkdir(parents=True)
    ledger = state / 'taskr.db'
    fixtures.clone(busy, ledger)
    fake = Herdr(home)
    # No inherited contract clock, client configuration, or real service tools.
    e = env(home, fake)
    e = {k: v for k, v in e.items() if not k.startswith('TASKR_CONTRACT_') and k != 'TASKR_FROZEN_NOW'}
    e['TASKR_DB'] = str(ledger)
    (home / 'bin/tailscale').write_text('#!/bin/sh\nexit 2\n')
    (home / 'bin/tailscale').chmod(0o755)
    (state / 'dashboard.addr').write_text('127.0.0.1:0\n')
    active = home / 'taskr'; install(go, active)
    p = None; detached = None; lock = state / 'daemon.lock'
    try:
        top = record(active, e, 'new', 'handoff-local', '--role', 'orchestrator')['task_id']
        record(active, e, 'note', 'written by Go before swap', '--as', str(top))
        original_schema = schema(ledger)
        p = start(active, e, '--stay')
        assert lock_held(lock) and int(lock.read_text()) == p.pid
        inode = lock.stat().st_ino
        passed(STEPS[0])
        old = p.pid
        install(rust, active)  # Old Go is still running its unlinked executable.
        assert p.poll() is None
        reply = record(active, e, 'daemon', '--restart')
        detached = reply['new_pid']
        assert reply['old_pid'] == old and reply['restarted'] and detached != old, reply
        stop(p); p = None
        assert lock.stat().st_ino == inode and lock_held(lock)
        assert int(lock.read_text()) == detached
        contender = record(active, e, 'daemon')
        assert contender['already_running'] and contender['pid'] == detached
        passed(STEPS[1], reply)
        code, log, err = invoke(active, e, '--json', 'log', str(top))
        assert code == 0 and b'written by Go before swap' in log, (code, log, err)
        record(active, e, 'note', 'written by default Rust', '--as', str(top))
        assert schema(ledger) == original_schema
        record(go, e, 'status')  # openDB runs the real Go migration on Rust's ledger.
        assert schema(ledger) == original_schema
        assert scalar(ledger, "select count(*) from events where summary='written by default Rust'") == 1
        passed(STEPS[2])
        rust_pid = detached
        install(go, active)
        reply = record(active, e, 'daemon', '--restart')
        detached = reply['new_pid']
        assert reply['old_pid'] == rust_pid and reply['restarted'] and detached != rust_pid, reply
        assert lock.stat().st_ino == inode and lock_held(lock) and int(lock.read_text()) == detached
        assert record(active, e, 'daemon')['already_running']
        code, log, err = invoke(active, e, '--json', 'log', str(top))
        assert code == 0 and b'written by default Rust' in log, (code, log, err)
        record(active, e, 'note', 'written by Go after rollback', '--as', str(top))
        code, log, err = invoke(rust, e, '--json', 'log', str(top))
        assert code == 0 and b'written by Go after rollback' in log, (code, log, err)
        assert schema(ledger) == original_schema
        passed(STEPS[3], reply)
    finally:
        try:
            if p is not None: stop(p)
        finally:
            try:
                # A restart/start may have succeeded before its reply was parsed.
                if lock.exists() and lock_held(lock):
                    stop_detached(int(lock.read_text()), lock)
            finally:
                fake.close()


def rpc(go, rust, place, busy, passed):
    place.mkdir()
    fixtures.clone(busy, place / 'hub.db')
    active = place / 'taskr'; install(go, active)
    cell = Cell(active, rust, place)
    fake = Herdr(cell.client_home)
    cell.client_env.update(HERDR_SOCKET_PATH=str(fake.path), PATH=str(cell.client_home / 'bin') + ':' + cell.env['PATH'])
    relay = None; waiter = None; proxy = None
    lock = cell.hub_home / '.local/state/taskr/daemon.lock'
    queue = cell.client_home / '.local/state/taskr/spool/queue'
    try:
        original_schema = schema(cell.db)
        top = cell.obj(go, ['new', 'handoff-rpc', '--role', 'orchestrator'])['task_id']
        lane = cell.obj(go, ['new', 'handoff-worker', '--role', 'implementer', '--parent', str(top), '--pane', 'wFixture:p1'])['task_id']
        launch = cell.obj(go, ['launch', str(lane), '--provider', 'fixture', '--model', 'fixture', '--effort', 'medium'])['launch_id']
        worker = {'TASKR_TASK': str(lane), 'TASKR_LAUNCH': str(launch)}
        (cell.client_home / 'agents.json').write_text(json.dumps([{'name': 'handoff-worker', 'pane_id': 'wFixture:p1', 'agent_status': 'working', 'state_change_seq': 1}]))
        relay = start(go, cell.client_env)
        eventually(lambda: scalar(cell.db, "select count(*) from meta where key='daemon_heartbeat:host-a'") == 1)
        key = 'handoff-dedupe-forward'
        argv = ['--json', '--request-key', key, 'note', 'one stored reply across swap', '--as', str(top)]
        stored = cell.want(go, argv)
        stored_hash = scalar(cell.db, 'select argv_sha from requests where key=?', (key,))
        # Reuse the cell's lost-reply boundary: hold a completed wait response
        # before the caller receives it, then stop its hub and replay in Rust.
        proxy = InterruptProxy(cell.url, before=False)
        cell.server_file.write_text(proxy.url + '\n')
        waiter = subprocess.Popen([str(go), '--json', 'wait', '--as', str(top), '--for', 'ready', '--timeout', '60000'],
                                  cwd=place, env=cell.client_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        eventually(lambda: scalar(cell.db, 'select waiting_until is not null from tasks where id=?', (top,)) == 1)
        cell.server_file.write_text(cell.url + '\n')
        ready = cell.obj(go, ['ready', 'in-flight handoff event'], env=worker)['event_id']
        assert proxy.hit.wait(10) and proxy.error is None, proxy.error
        assert waiter.poll() is None
        assert scalar(cell.db, 'select pending_event_id from tasks where id=?', (top,)) == ready
        passed(STEPS[4], {'pending_event': ready, 'request_hash': stored_hash})
        stop(relay); relay = None
        stop(cell.hub)
        assert not lock_held(lock) and lock.read_bytes() == b''
        proxy.close(); proxy = None
        waiter.terminate(); waiter.communicate(timeout=10); waiter = None
        for index in range(2):
            queued = cell.obj(go, ['--request-key', f'handoff-go-queue-{index}', 'note', f'Go spool {index}', '--as', str(top)])
            assert queued['queued'], queued
        assert len(list(queue.glob('*.json'))) == 2
        port = cell.url.rsplit(':', 1)[1]
        (cell.hub_home / '.local/state/taskr/dashboard.addr').write_text('tailnet:' + port + '\n')
        install(rust, active)
        cell.hub = start(active, cell.hub_env, '--stay')
        assert lock_held(lock) and int(lock.read_text()) == cell.hub.pid
        passed(STEPS[5])
        relay = start(rust, cell.client_env)
        fake.wake()
        eventually(lambda: not list(queue.glob('*.json')))
        for index in range(2):
            assert scalar(cell.db, 'select count(*) from events where summary=?', (f'Go spool {index}',)) == 1
        for client in (go, rust):
            replay = cell.obj(client, ['wait', '--as', str(top), '--for', 'ready', '--timeout', '0'])
            assert replay['event']['id'] == ready and replay['replay'], replay
            reply = cell.want(client, argv)
            assert (reply.stdout, reply.stderr) == (stored.stdout, stored.stderr), (client, reply, stored)
            assert scalar(cell.db, 'select argv_sha from requests where key=?', (key,)) == stored_hash
            assert scalar(cell.db, "select count(*) from events where summary='one stored reply across swap'") == 1
        reverse_argv = ['--json', '--request-key', 'handoff-dedupe-reverse', 'note', 'Rust reply survives rollback', '--as', str(top)]
        reverse_stored = cell.want(rust, reverse_argv)
        passed(STEPS[6], {'event_id': ready, 'replay': replay['replay'], 'clients': ['Go', 'Rust']})
        stop(relay); relay = None
        stop(cell.hub)
        assert not lock_held(lock) and lock.read_bytes() == b''
        for index in range(2):
            queued = cell.obj(rust, ['--request-key', f'handoff-rust-queue-{index}', 'note', f'Rust spool {index}', '--as', str(top)])
            assert queued['queued'], queued
        assert len(list(queue.glob('*.json'))) == 2
        # Reopen this ledger; never copy the original fixture back on rollback.
        current_schema = schema(cell.db)
        record(go, cell.hub_env, 'status')
        assert schema(cell.db) == current_schema == original_schema
        install(go, active)
        cell.hub = start(active, cell.hub_env, '--stay')
        passed(STEPS[7])
        relay = start(go, cell.client_env); fake.wake()
        eventually(lambda: not list(queue.glob('*.json')))
        for index in range(2):
            assert scalar(cell.db, 'select count(*) from events where summary=?', (f'Rust spool {index}',)) == 1
        assert b'Rust reply survives rollback' in cell.want(go, ['--json', 'log', str(top)]).stdout
        reply = cell.want(go, reverse_argv)
        assert (reply.stdout, reply.stderr) == (reverse_stored.stdout, reverse_stored.stderr)
        assert scalar(cell.db, "select count(*) from events where summary='Rust reply survives rollback'") == 1
        record(rust, cell.hub_env, 'status')
        assert schema(cell.db) == original_schema
        assert not (cell.client_home / '.local/state/taskr/taskr.db').exists()
        passed(STEPS[8])
    finally:
        if waiter is not None:
            waiter.terminate(); waiter.communicate(timeout=10)
        if proxy is not None: proxy.close()
        if relay is not None: stop(relay)
        fake.close(); cell.close()


def cleanup_probes(go, rust, place, busy):
    from unittest.mock import patch
    checks = []
    for boundary in ('start', 'restart'):
        spawned = []
        real_start, real_record = start, record
        def broken_start(*args):
            process = real_start(*args); spawned.append(process)
            raise RuntimeError('synthetic startup reply failure')
        def broken_record(*args):
            reply = real_record(*args)
            if args[2:4] == ('daemon', '--restart'):
                raise RuntimeError('synthetic restart reply failure')
            return reply
        lane = place / boundary
        try:
            with patch(__name__ + ('.start' if boundary == 'start' else '.record'),
                       broken_start if boundary == 'start' else broken_record):
                try:
                    local(go, rust, lane, busy, lambda *args: None)
                except RuntimeError as error:
                    assert str(error) == f'synthetic {"startup" if boundary == "start" else "restart"} reply failure', error
                else:
                    raise AssertionError('failure probe did not reach its boundary')
        finally:
            for process in spawned:
                process.communicate(timeout=5)
        lock = lane / 'home/.local/state/taskr/daemon.lock'
        assert not lock_held(lock) and lock.read_bytes() == b''
        checks.append(boundary + ' reply failure releases the scratch daemon lock')
    return checks


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--rust', type=Path, required=True, help='default-feature candidate')
    p.add_argument('--rust-contract', type=Path, required=True, help='same candidate with contract fixtures enabled')
    p.add_argument('--go', type=Path); p.add_argument('--go-contract', type=Path)
    p.add_argument('--out', type=Path, required=True)
    p.add_argument('--work-dir', type=Path, help='new scratch directory to retain evidence; must not exist')
    a = p.parse_args()
    if sys.platform != 'linux': p.error('Linux required for daemon --restart')
    a.rust = a.rust.resolve(); a.rust_contract = a.rust_contract.resolve()
    for binary in (a.rust, a.rust_contract):
        if not binary.is_file(): p.error(f'missing binary: {binary}')
    default_strings = subprocess.run(['strings', '-a', str(a.rust)], capture_output=True, check=True).stdout
    assert not any(hook in default_strings for hook in (b'TASKR_FROZEN_NOW', b'TASKR_CONTRACT_', b'--contract-')), 'default candidate contains contract hooks'
    rows = []
    def passed(name, evidence=None):
        rows.append({'step': name, 'status': 'PASS', 'evidence': evidence})
    temp = None
    if a.work_dir:
        place = a.work_dir.resolve(); place.mkdir(parents=True, exist_ok=False)
    else:
        temp = tempfile.TemporaryDirectory(prefix='taskr-handoff-'); place = Path(temp.name)
    error = None; cleanup_checks = []
    try:
        oracles = []
        for supplied, name, tags in ((a.go, 'go-default', []), (a.go_contract, 'go-contract', ['-tags', 'taskr_contract'])):
            binary = supplied.resolve() if supplied else place / name
            if supplied is None:
                subprocess.run(['go', 'build', *tags, '-trimpath', '-ldflags', '-s -w -X main.version=v0.16.1', '-o', str(binary), '.'], cwd=ROOT, check=True)
            oracles.append(binary)
        corpus = place / 'corpus'; corpus.mkdir()
        busy = fixtures.fixtures(corpus, None)['busy']
        cleanup_checks = cleanup_probes(oracles[0], a.rust, place / 'cleanup', busy)
        local(oracles[0], a.rust, place / 'local', busy, passed)
        rpc(oracles[1], a.rust_contract, place / 'rpc', busy, passed)
    except Exception:
        error = traceback.format_exc()
    finally:
        for index, name in enumerate(STEPS[len(rows):]):
            rows.append({'step': name, 'status': 'FAIL' if index == 0 else 'NOT RUN', 'error': error})
        payload = {'passed': sum(r['status'] == 'PASS' for r in rows), 'failed': error is not None,
                   'cleanup_checks': cleanup_checks,
                   'default_rpc': 'NOT EXERCISED: first exercised at real swap over real tailnet',
                   'candidates': {str(b): hashlib.sha256(b.read_bytes()).hexdigest() for b in (a.rust, a.rust_contract)},
                   'rows': rows, 'error': error}
        a.out.write_text(json.dumps(payload, indent=2) + '\n')
        print('RESULT    CHECK')
        for row in rows: print(f"{row['status']:<9} {row['step']}")
        if error: print(error, file=sys.stderr)
        if temp is not None: temp.cleanup()
    return 1 if error else 0


if __name__ == '__main__':
    sys.exit(main())
