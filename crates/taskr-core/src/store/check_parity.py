#!/usr/bin/env python3
"""Synthetic write sequences; compare byte channels and logical DB after every command.
Run from repository root: python3 crates/taskr-core/src/store/check_parity.py
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import sys
import subprocess
import tempfile
import threading

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[4]
spec = importlib.util.spec_from_file_location('contract', ROOT / 'tools/contract/run.py')
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)
GO = ROOT / '.scratch/taskr-oracle'
RUST = ROOT / '.scratch/taskr-write-candidate'
results = []

with tempfile.TemporaryDirectory(prefix='write-sequences-', dir=ROOT / '.scratch') as tmp:
    root = Path(tmp)
    home = root / 'home'
    home.mkdir()
    fake = home / 'bin'
    fake.mkdir()
    executable = fake / 'herdr'
    executable.write_text('#!/bin/sh\nprintf \'{"result":{"agent":{"agent_status":"working"}}}\\n\'\n')
    executable.chmod(0o755)
    brief = root / 'brief.md'
    brief.write_text('HERDR-BRIEF role=implementer\nSynthetic <&> Þ😀\n')
    report = root / 'report.md'
    report.write_text('Synthetic report <&> Þ😀\n')
    plan = root / 'plan.md'
    plan.write_text('# Synthetic plan\nOne\nTwo\nThree\n')
    sock = root / 'fake.sock'
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(str(sock))
    listener.listen()
    listener.settimeout(.1)
    stop = threading.Event()
    def accept():
        while not stop.is_set():
            try:
                conn, _ = listener.accept()
                conn.close()
            except socket.timeout:
                continue
            except OSError:
                break
    thread = threading.Thread(target=accept, daemon=True)
    thread.start()
    env = {'HOME': str(home), 'PATH': str(fake) + ':/usr/bin:/bin',
           'HERDR_SOCKET_PATH': str(sock), 'TASKR_FROZEN_NOW': '2026-10-08T00:00:00Z',
           'TASKR_CONTRACT_ORACLE': '1', 'LANG': 'C.UTF-8', 'TZ': 'UTC'}
    db = root / 'ledger.db'
    previous = root / 'previous.db'
    variables = {'brief': str(brief), 'report': str(report), 'plan': str(plan), 'root': '1', 'worker': '2'}
    worker_env = {'TASKR_TASK': '2', 'TASKR_LAUNCH': '{launch}', 'HERDR_PANE_ID': 'w9:p1'}
    def step(argv, extra=None, capture=None, stdin=None):
        argv = [s.format(**variables) for s in argv]
        extra = {k: v.format(**variables) for k, v in (extra or {}).items()}
        if not db.exists():
            sqlite3.connect(db).close()
        previous.unlink(missing_ok=True)
        contract.clone(db, previous)
        outcomes, states = [], []
        for binary in [GO, RUST]:
            for suffix in ['', '-wal', '-shm']:
                Path(str(db) + suffix).unlink(missing_ok=True)
            contract.clone(previous, db)
            process = subprocess.run([str(binary), *argv], env={**env, 'TASKR_DB': str(db), **extra},
                                     input=stdin, capture_output=True, timeout=5, cwd=ROOT)
            outcomes.append((process.returncode, process.stdout, process.stderr))
            states.append(contract.logical(db))
        differences = [name for name, a, b in zip(['exit', 'stdout', 'stderr'], outcomes[0], outcomes[1]) if a != b]
        if states[0] != states[1]:
            differences.append('logical-db')
        record = {'argv': argv, 'status': 'mismatch' if differences else 'pass', 'differences': differences}
        if differences:
            record.update(go={'exit': outcomes[0][0], 'stdout': repr(outcomes[0][1]), 'stderr': repr(outcomes[0][2])},
                          rust={'exit': outcomes[1][0], 'stdout': repr(outcomes[1][1]), 'stderr': repr(outcomes[1][2])})
            print(json.dumps(record, ensure_ascii=False), flush=True)
        results.append(record)
        if capture:
            response = json.loads(outcomes[0][1])
            for key, field in capture.items():
                variables[key] = str(response[field])
        return outcomes

    step(['--json', 'new', 'root', '--role', 'orchestrator', '--brief', '{brief}', '--pane', 'w9:p0'])
    step(['new', 'worker', '--role', 'implementer', '--parent', '1', '--brief', '{brief}', '--report', '{report}', '--pane', 'w9:p1'])
    step(['--json', 'launch', '2', '--provider', 'claude', '--model', 'synthetic', '--effort', 'medium'], capture={'launch': 'launch_id'})
    step(['start'], worker_env)
    step(['note', 'synthetic <&> Þ😀', '--key', 'note-one'], worker_env)
    step(['--json', 'note', 'retry different text', '--key', 'note-one'], worker_env)
    step(['ask', 'wrong-kind', '--key', 'note-one'], worker_env)
    step(['note', 'reserved', '--key', 'got:1'], worker_env)
    step(['--json', 'ask', 'question <&> Þ😀', '--blocking'], worker_env, {'ask': 'ask_id'})
    step(['wait', '--as', '1', '--for', 'ask', '--timeout', '0'])
    step(['ack', '{ask}', '--as', '1'])
    step(['ack', '{ask}', '--as', '1'])
    step(['answer', '{ask}', 'wrong recipient', '--as', '2'])
    step(['--json', 'answer', '{ask}', 'answer <&> Þ😀', '--as', '1'], capture={'answer': 'answer_id'})
    step(['wait', '--for', 'answer', '--timeout', '0'], worker_env)
    step(['ack', '{answer}', '--as', '2'], worker_env)
    step(['ready', 'slice', '--report', '{report}', '--kv', 'n=1', '--key', 'ready-one'], worker_env)
    step(['done', 'finished'], worker_env)
    step(['--json', 'prompt', '2', '--text', 'Again <&> Þ😀'], capture={'attempt': 'attempt_id'})
    step(['--json', 'got', '{attempt}'], worker_env, {'got': 'event_id'})
    step(['got', '{attempt}'], worker_env)
    step(['--json', 'prompt', '2', '--file', '{brief}', '--receipt-timeout', '0'], capture={'attempt': 'attempt_id'})
    step(['got', '{attempt}'], worker_env)
    step(['--json', 'prompt', '2', '--text', 'Confirm', '--confirm', '--confirm-timeout', '0'], capture={'attempt': 'attempt_id'})
    step(['got', '{attempt}'], worker_env)
    step(['--json', 'prompt', '2', '--text', 'Overflow receipt', '--receipt-timeout', '9223372036854775807'])
    step(['--json', 'prompt', '2', '--text', 'Overflow confirm', '--confirm', '--confirm-timeout', '9223372036854775807'])
    step(['wait', '--as', '1', '--for', 'answer', '--timeout', '9223372036854775807'])
    step(['set', '2', 'pr=1', 'branch=feature'])
    step(['set', '2', 'pr=1', 'branch=feature'])
    step(['set', '2', 'pr=', 'branch=updated'])
    step(['next', '2', 'Proceed <&> Þ😀'])
    step(['next', '2', '--clear'])
    step(['--json', 'decide', '--as', '1', 'Choice <&> Þ😀'], capture={'decision': 'event_id'})
    step(['decide', '--as', '1', '--revoke', '{decision}'])
    step(['decide', '--as', '1', '--revoke', '{decision}'])
    step(['--json', 'ask', 'Owner decision', '--owner'], worker_env, {'ask': 'ask_id'})
    step(['answer', '{ask}', 'Approved', '--as', '1'])
    step(['decide', '--as', '1', '--revoke', '{ask}'])
    step(['--json', 'doc', 'set', '1', 'plan', '--file', '{plan}'], capture={'doc': 'doc_id'})
    step(['doc', 'set', '1', 'plan', '--file', '{plan}'])
    step(['doc', 'set', '1', 'plan', '--name', 'named-plan', '--file', '{plan}'])
    step(['handover', '--as', '1', '--note', 'Handoff <&> Þ😀'])
    step(['adopt', '1', '--pane', 'w2:p0'])
    step(['handover', '--as', '1'])
    step(['doc', 'rm', '{doc}', '--purge'])
    step(['doc', 'backfill', '--tree', '1', '--dry-run'])
    step(['doc', 'backfill', '--tree', '1'])
    step(['fail', 'Failed'], worker_env)
    step(['new', 'future', '--role', 'implementer', '--parent', '1', '--planned', '--cwd', '/missing'])
    step(['launch', '3', '--provider', 'codex', '--model', 'm', '--effort', 'high', '--workspace', 'w3', '--pane', 'w3:p1'])
    step(['close', '3', '--outcome', 'abandoned'])
    step(['--json', 'launch', '2', '--provider', 'claude', '--model', 'synthetic', '--effort', 'high'], capture={'launch': 'launch_id'})
    step(['note', 'stale identity'], {'TASKR_TASK': '2', 'TASKR_LAUNCH': '1'})
    step(['hook', 'claude', 'SessionStart'], {**worker_env, 'HERDR_ENV': '1'}, stdin=b'{"session_id":"synthetic-session"}')
    step(['--json', 'prompt', '2', '--text', 'Hook receipt'], capture={'attempt': 'attempt_id'})
    payload = json.dumps({'session_id': 'synthetic-session', 'prompt': 'First taskr got ' + variables['attempt'] + '. Hook'}).encode()
    step(['hook', 'claude', 'UserPromptSubmit'], {**worker_env, 'HERDR_ENV': '1'}, stdin=payload)
    step(['hook', 'claude', 'Stop'], {**worker_env, 'HERDR_ENV': '1'}, stdin=b'{"session_id":"synthetic-session"}')
    step(['hook', 'claude', 'Stop'], {**worker_env, 'HERDR_ENV': '1'}, stdin=b'{"session_id":"synthetic-session"}')
    step(['ask', 'Unanswered owner ask', '--owner'], worker_env)
    step(['close', '2', '--outcome', 'rejected'])
    step(['close', '2', '--outcome', 'accepted'])
    step(['note', 'closed task'], worker_env)
    step(['close', '1'])
    stop.set()
    listener.close()
    thread.join(timeout=1)

output = ROOT / '.scratch/r1-write-sequences.json'
output.write_text(json.dumps({'results': results, 'pass': sum(r['status'] == 'pass' for r in results),
                              'mismatch': sum(r['status'] == 'mismatch' for r in results)}, indent=2) + '\n')
print(f"write sequences: {len(results)} cases; {sum(r['status']=='mismatch' for r in results)} mismatches")
raise SystemExit(any(r['status'] == 'mismatch' for r in results))
