#!/usr/bin/env python3
"""Native macOS scratch-only restart, Go/Rust identity and inherited HUP proof."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import subprocess
import sys
from daemon_cell import Herdr, env, eventually, invoke, record, start, stop


def locked(path):
    with path.open('a+b') as file:
        try:
            fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        return False


def install(source, target):
    staged = target.with_suffix('.next')
    shutil.copy2(source, staged)
    staged.chmod(0o755)
    os.replace(staged, target)


def run(go, rust, root):
    results = []
    for mode in ('local', 'client'):
        home = root / mode
        home.mkdir()
        state = home / '.local/state/taskr'
        state.mkdir(parents=True)
        (state / 'dashboard.addr').write_text('off')
        fake = Herdr(home)
        environment = env(home, fake)
        environment.pop('TASKR_FROZEN_NOW')
        (home / 'bin/tailscale').write_text('#!/bin/sh\nexit 2\n')
        (home / 'bin/tailscale').chmod(0o755)
        if mode == 'local':
            environment['TASKR_DB'] = str(state / 'taskr.db')
        else:
            environment.pop('TASKR_DB')
            (state / 'server.url').write_text('http://127.0.0.1:9\n')
        active = home / 'taskr'
        lock = state / 'daemon.lock'
        identity = state / ('client-daemon.json' if mode == 'client' else 'daemon.json')
        child = None
        try:
            install(go, active)
            child = start(active, environment)
            inode = lock.stat().st_ino
            for replacement, label in ((rust, 'Go record accepted by Rust'),
                                       (go, 'Rust record accepted by Go'),
                                       (rust, 'Go record accepted by Rust again'),
                                       (rust, 'Rust restart')):
                old = int(lock.read_text())
                written = json.loads(identity.read_bytes())
                assert written['pid'] == old and written['uid'] == os.geteuid(), written
                assert written['argv'] == [str(active), 'daemon'], written
                assert written['executable'] == str(active.resolve()), written
                sec, usec = written['start_time'].split('.')
                assert int(sec) > 0 and len(usec) == 6 and 0 <= int(usec) < 1_000_000
                install(replacement, active)
                reply = record(active, environment, 'daemon', '--restart')
                assert reply['restarted'] and reply['old_pid'] == old and reply['new_pid'] != old, reply
                assert locked(lock) and lock.stat().st_ino == inode
                assert int(lock.read_text()) == reply['new_pid']
                if replacement.samefile(rust):
                    child_env = subprocess.check_output(['ps','eww','-p',str(reply['new_pid'])],text=True)
                    assert 'TASKR_TMP_BASE='+environment['TASKR_TMP_BASE'] in child_env, child_env
                if child is not None:
                    stop(child)
                    child = None
                assert record(active, environment, 'daemon')['already_running']
                results.append({'mode': mode, 'check': label, 'record': written, 'reply': reply})
            if mode == 'client':
                original = identity.read_bytes()
                resident = json.loads(original)['pid']
                for field, wrong in (('pid', 1), ('executable', '/absent/taskr'),
                                     ('argv', [str(active), 'daemon', '--stay']),
                                     ('start_time', '1.000000'), ('uid', os.geteuid() + 1)):
                    forged = json.loads(original)
                    forged[field] = wrong
                    identity.write_text(json.dumps(forged))
                    code, output, error = invoke(active, environment, 'daemon', '--restart')
                    assert code == 6 and b'not signalled' in output + error, (field, code, output, error)
                    assert locked(lock) and int(lock.read_text()) == resident
                    os.kill(resident, 0)
                    identity.write_bytes(original)
                    results.append({'mode': mode, 'check': 'forged ' + field + ' refused'})
                assert not (state / 'taskr.db').exists()
            os.kill(int(lock.read_text()), signal.SIGTERM)
            eventually(lambda: not locked(lock))
            assert lock.read_bytes() == b'' and not identity.exists()
            if mode == 'client':
                sleeper = subprocess.Popen(['/bin/sleep', '30'])
                try:
                    with lock.open('w') as held:
                        fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        held.write(str(sleeper.pid) + '\n')
                        held.flush()
                        forged = json.loads(original)
                        forged['pid'] = sleeper.pid
                        identity.write_text(json.dumps(forged))
                        code, output, error = invoke(active, environment, 'daemon', '--restart')
                        assert code == 6 and b'does not run the recorded daemon executable' in output + error, (code, output, error)
                        assert sleeper.poll() is None and locked(lock)
                        assert int(lock.read_text()) == sleeper.pid
                        results.append({'mode': mode, 'check': 'live non-taskr PID refused at executable check'})
                finally:
                    sleeper.terminate()
                    sleeper.wait(timeout=5)
                    identity.unlink(missing_ok=True)
                    lock.write_text('')
            for ignored in (False, True):
                shell = "trap '' HUP; exec \"$@\"" if ignored else 'trap - HUP; exec "$@"'
                child = subprocess.Popen(['/bin/sh', '-c', shell, 'fixture', str(active), 'daemon'],
                                         env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                selector = selectors.DefaultSelector()
                selector.register(child.stdout, selectors.EVENT_READ)
                assert selector.select(15), 'HUP daemon startup timeout'
                assert json.loads(child.stdout.readline().removeprefix(b'j1 '))['pid'] == child.pid
                selector.close()
                log = state / 'daemon.log'
                log_start = log.stat().st_size
                os.kill(child.pid, signal.SIGHUP)
                if ignored:
                    try:
                        child.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        pass
                    else:
                        raise AssertionError('inherited ignored HUP stopped daemon')
                    assert locked(lock)
                    assert b'exit: signal' not in log.read_bytes()[log_start:]
                    stop(child)
                else:
                    child.communicate(timeout=10)
                    assert child.returncode == 0, child.returncode
                child = None
                assert not locked(lock) and lock.read_bytes() == b''
                results.append({'mode': mode, 'check': 'HUP ignored' if ignored else 'HUP exits cleanly'})
        finally:
            if child is not None:
                stop(child)
            if lock.exists() and locked(lock):
                os.kill(int(lock.read_text()), signal.SIGTERM)
                eventually(lambda: not locked(lock))
            fake.close()
    return results


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--go', type=Path, required=True)
    parser.add_argument('--rust', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    if sys.platform != 'darwin':
        parser.error('macOS required')
    args.work_dir.mkdir()
    checks = run(args.go.resolve(), args.rust.resolve(), args.work_dir.resolve())
    args.out.write_text(json.dumps({'passed': len(checks), 'checks': checks}, indent=2) + '\n')
    print(json.dumps({'passed': len(checks)}))
