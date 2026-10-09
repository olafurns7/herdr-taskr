#!/usr/bin/env python3
"""Byte and logical-SQLite parity. Requires two explicit binaries; never uses installed taskr."""
import argparse
from collections import defaultdict
import hashlib
import importlib
import json
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import golden

ROOT = Path(__file__).resolve().parents[2]
NOT_IMPLEMENTED = 125


def quote(name):
    return '"' + name.replace('"', '""') + '"'


def logical(path, normalized=False):
    """Include visible FTS rows and application schema, exclude implementation shadow tables."""
    if not path.exists():
        return None
    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as db:
        db.execute('pragma temp_store=MEMORY')
        shadow = {row[1] for row in db.execute("pragma table_list") if row[2] == "shadow"}
        schema = list(db.execute("select type,name,tbl_name,sql from sqlite_master order by type,name"))
        schema = [row for row in schema if row[1] not in shadow and row[2] not in shadow]
        tables = {}
        for name, in db.execute("select name from sqlite_master where type='table' order by name"):
            if name in shadow:
                continue
            columns = [row[1] for row in db.execute(f"pragma table_info({quote(name)})")]
            fields = ','.join(map(quote, columns))
            if name == 'search_fts':
                fields = 'rowid,' + fields
            digest, count = hashlib.sha256(), 0
            for row in db.execute(f"select {fields} from {quote(name)} order by {fields}"):
                values = [{"blob": value.hex()} if isinstance(value, bytes) else value for value in row]
                digest.update(json.dumps(golden.normalize(values) if normalized else values, ensure_ascii=True, separators=(',', ':')).encode() + b'\n')
                count += 1
            tables[name] = {"columns": columns, "rows": count, "sha256": digest.hexdigest()}
        integrity = list(db.execute('pragma integrity_check'))
        foreign_keys = list(db.execute('pragma foreign_key_check'))
        if integrity != [('ok',)] or foreign_keys:
            raise RuntimeError(f"fixture integrity failure: {integrity}, FK violations={len(foreign_keys)}")
        return {"schema": schema, "tables": tables}


def clone(source, target):
    with sqlite3.connect(f"file:{source}?mode=ro", uri=True) as src, sqlite3.connect(target) as dest:
        src.backup(dest)


def fixtures(directory, live):
    paths = {}
    for name in ('empty', 'legacy', 'busy'):
        path = directory / (name + '.db')
        with sqlite3.connect(path) as db:
            if name == 'legacy':
                db.executescript((ROOT / 'testdata/taskr-v0.9.1-schema.sql').read_text())
            if name == 'busy':
                db.executescript((ROOT / 'schema.sql').read_text())
                stamp = '2026-10-07T23:00:00.000Z'
                for i in range(1, 81):
                    parent = None if i == 1 else 1 if i < 10 else 2
                    status = 'closed' if i % 7 == 0 else 'open'
                    db.execute('insert into tasks(id,parent_id,name,role,status,created_at,updated_at) values(?,?,?,?,?,?,?)',
                               (i, parent, f'lane {i} <&> Þ😀', 'orchestrator' if i == 1 else 'implementer', status, stamp, stamp))
                    if i > 1:
                        db.execute('insert into launches(id,task_id,provider,model,recorded_at) values(?,?,?,?,?)', (i, i, 'fixture', 'fixture', stamp))
                        db.execute('update tasks set current_launch_id=? where id=?', (i, i))
                    db.execute('insert into events(task_id,recipient_task_id,kind,summary,data,created_at) values(?,?,?,?,?,?)',
                               (i, 1, 'ask' if i % 3 == 0 else 'note', f'fixture {i} <&> Þ😀', '{"n":9007199254740993,"minus":-0.0}', stamp))
                body = '<goal> Þ😀\n'
                sha = hashlib.sha256(body.encode()).hexdigest()
                db.execute('insert into doc_blobs values(?,?,?)', (sha, len(body.encode()), body))
                db.execute("insert into documents(root_id,task_id,kind,version,sha256,bytes,captured,created_at) values(1,1,'goal',1,?,?,1,?)", (sha, len(body.encode()), stamp))
        paths[name] = path
    if live:
        paths['live-ledger'] = live.resolve()
    return paths


def execute(binary, args, home, db, extra_env, timeout, client_url=None, child_mode=None, caller=None):
    home.mkdir()
    fake = home / 'bin'
    fake.mkdir()
    for name in ('herdr', 'tailscale'):
        script = fake / name
        script.write_text('#!/bin/sh\necho "contract fixture: service unavailable" >&2\nexit 2\n')
        script.chmod(0o755)
    state = home / '.local/state/taskr'
    state.mkdir(parents=True)
    (state / 'dashboard.addr').write_text('127.0.0.1:7788\n')
    env = {"HOME": str(home), "TASKR_DB": str(db), "PATH": str(fake) + ':/usr/bin:/bin',
           "HERDR_SOCKET_PATH": str(home / 'absent.sock'), "TASKR_FROZEN_NOW": '2026-10-08T00:00:00Z',
           "TASKR_CONTRACT_ORACLE": '1', "LANG": 'C.UTF-8', "TZ": 'UTC',
           "SQLITE_TMPDIR": str(home), **extra_env}
    if client_url:
        state = home / '.local/state/taskr'
        state.mkdir(parents=True, exist_ok=True)
        (state / 'server.url').write_text(client_url + '\n')
        env['TASKR_DB'] = ''
    input_bytes = None
    if caller:
        env.update(TASKR_RPC_CALLER=caller, TASKR_RPC_CWD=str(home))
        request = {'argv': args, 'cwd': str(home), 'env': {}, 'request_key': 'synthetic-child'}
        input_bytes = json.dumps(request).encode()
        args = ['--contract-rpc-child', caller] if child_mode == 'go' else ['--hub-child', *args]
    try:
        process = subprocess.run([str(binary), *args], env=env, input=input_bytes, capture_output=True, timeout=timeout, cwd=home.parent)
        return process.returncode, process.stdout, process.stderr
    except subprocess.TimeoutExpired:
        return -999, b'', b'contract: process timeout\n'


def main():
    global sqlite3
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--go', required=True, type=Path)
    parser.add_argument('--rust', required=True, type=Path)
    parser.add_argument('--family', action='append', help='exact family or read/write/net/schema prefix; repeatable')
    parser.add_argument('--catchup-fixtures', action='store_true', help='synthetic P1a/P1b trust, campaign and spark corpus')
    parser.add_argument('--cases', type=Path, default=ROOT / 'testdata/contract/cases.json')
    parser.add_argument('--live-snapshot', type=Path, help='already backed-up corpus; never the live ledger')
    parser.add_argument('--require-live', action='store_true')
    parser.add_argument('--out', type=Path)
    parser.add_argument('--timeout', type=float, default=30)
    parser.add_argument('--sqlite-module', choices=('sqlite3', 'pysqlite3'), default='sqlite3',
                        help='optional newer user-space SQLite for recent FTS5 indexes')
    options = golden.parse(parser, __file__)
    try:
        sqlite3 = importlib.import_module(options.sqlite_module)
    except ImportError as error:
        parser.error(str(error))
    if (options.record or options.expect) and options.live_snapshot:
        parser.error('goldens accept synthetic fixtures only')
    if options.require_live and not options.live_snapshot:
        parser.error('--require-live needs --live-snapshot')
    scratch_root = ROOT / '.scratch'
    scratch_root.mkdir(exist_ok=True)
    if options.live_snapshot and not options.live_snapshot.resolve().is_relative_to(scratch_root.resolve()):
        parser.error('live-ledger corpus must remain under this worktree .scratch/')
    if options.live_snapshot and options.out and not options.out.resolve().is_relative_to(scratch_root.resolve()):
        parser.error('live-corpus results must remain under this worktree .scratch/')
    go, rust = options.go.resolve(), options.rust.resolve()
    for binary in (go, rust):
        if not binary.is_file():
            parser.error(f'binary does not exist: {binary}')
    indexed_cases = list(enumerate(json.loads(options.cases.read_text())))
    if options.family:
        indexed_cases = [(i, case) for i, case in indexed_cases if any(case['family'] == family or case['family'].startswith(family + ':') for family in options.family)]
    cases = [case for _, case in indexed_cases]
    if golden.session:
        golden.session.require_all = options.catchup_fixtures and not options.family
    if not cases:
        parser.error('no cases selected')
    summary, results = defaultdict(lambda: defaultdict(int)), []
    groups = defaultdict(lambda: defaultdict(int))
    with tempfile.TemporaryDirectory(prefix='taskr-contract-', dir=scratch_root) as scratch:
        directory = Path(scratch)
        corpus = fixtures(directory, options.live_snapshot)
        if options.catchup_fixtures:
            from catchup_fixtures import fixtures as catchup_fixtures
            corpus.update(catchup_fixtures(directory, (ROOT / 'schema.sql').read_text()))
        baseline = {name: logical(path) for name, path in corpus.items()}
        # An internal RPC child always receives its hub's already-migrated ledger.
        child_corpus = {}
        if any(case.get('rpc_caller') for case in cases):
            child_dir = directory / 'rpc-fixtures'
            child_dir.mkdir()
            for name, source in corpus.items():
                path = child_dir / (name + '.db')
                clone(source, path)
                code, _, stderr = execute(rust if options.expect else go, ['--contract-migrate'], child_dir / (name + '-home'), path, {}, options.timeout)
                if code != 0:
                    raise RuntimeError(f'RPC fixture migration failed: {name}: {code}: {stderr!r}')
                child_corpus[name] = path
        for i, case in indexed_cases:
            for fixture_name, fixture in corpus.items():
                if fixture_name not in case.get('fixtures', corpus):
                    continue
                if case.get('rpc_caller'):
                    fixture = child_corpus[fixture_name]
                    before = logical(fixture)
                else:
                    before = baseline[fixture_name]
                root = directory / f'{i}-{fixture_name}'
                root.mkdir()
                # Both executions see exactly the same pathname/env; restore private state between them.
                db = root / 'ledger.db'
                outcomes, after = [], []
                for binary in ((go,) if options.record else (rust,) if options.expect else (go, rust)):
                    for old_home in root.glob('home*'):
                        shutil.rmtree(old_home)
                    for suffix in ('', '-wal', '-shm'):
                        Path(str(db) + suffix).unlink(missing_ok=True)
                    clone(fixture, db)
                    steps = case.get('sequence', [case['argv']])
                    executions = [execute(binary, argv, root / f'home{step}', db, case.get('env', {}), options.timeout, case.get('client_url'), 'go' if binary == go and not (golden.session and golden.session.oracle_is_rust) else 'rust', case.get('rpc_caller'))
                                  for step, argv in enumerate(steps)]
                    outcomes.append(executions)
                    after.append(logical(db, normalized=bool(golden.session)))
                    if golden.session:
                        golden.observe([i, fixture_name, steps], {'executions': executions,
                            'logical_db': golden.digest(after[-1])})
                if golden.session:
                    outcomes = [outcomes[0], outcomes[0]]
                    after = [after[0], after[0]]
                differences = []
                go_codes = [out[0] for out in outcomes[0]]
                rust_codes = [out[0] for out in outcomes[1]]
                if any(code < 0 or code == NOT_IMPLEMENTED for code in go_codes) or -999 in rust_codes:
                    status = 'mismatch'
                    differences.append('oracle-unavailable-or-process-timeout')
                elif NOT_IMPLEMENTED in rust_codes:
                    status = 'not-implemented'
                    if after[1] != before:
                        status = 'mismatch'
                        differences.append('not-implemented command mutated DB')
                else:
                    for step, (lhs, rhs) in enumerate(zip(outcomes[0], outcomes[1])):
                        for field, a, b in zip(('exit', 'stdout', 'stderr'), lhs, rhs):
                            if a != b:
                                differences.append(f'{step}:{field}' if 'sequence' in case else field)
                    if after[0] != after[1]:
                        differences.append('logical-db')
                    status = 'mismatch' if differences else 'pass'
                family = case['family']
                summary[family][status] += 1
                case_groups = list(case.get('groups', []))
                for group in case_groups:
                    groups[group][status] += 1
                result = {'family': family, 'fixture': fixture_name, 'argv': case.get('sequence', case['argv']), 'status': status,
                          'groups': case_groups, 'go_exit': None if golden.session else go_codes if 'sequence' in case else go_codes[0], 'rust_exit': None if options.record else rust_codes if 'sequence' in case else rust_codes[0], 'differences': differences}
                if options.record:
                    result['record_exit'] = go_codes if 'sequence' in case else go_codes[0]
                results.append(result)
                if status == 'mismatch':
                    print(json.dumps(result, ensure_ascii=True))
                shutil.rmtree(root)
        payload = {'fixtures': list(corpus), 'summary': dict(summary), 'groups': dict(groups), 'results': results,
                   'mode': 'record' if options.record else 'expect' if options.expect else 'differential',
                   'go': None if golden.session else str(go), 'rust': None if options.record else str(rust),
                   'record_binary': str(options.record.resolve()) if options.record else None, 'clock': '2026-10-08T00:00:00Z', 'sqlite_version': sqlite3.sqlite_version}
    for family, counts in sorted(summary.items()):
        print(f"{family}: pass={counts['pass']} not-implemented={counts['not-implemented']} mismatch={counts['mismatch']}")
    if options.out:
        options.out.write_text(json.dumps(payload, indent=2) + '\n')
    failed = any(result['status'] == 'mismatch' for result in results)
    if not failed:
        golden.finish()
    return int(failed)


if __name__ == '__main__':
    raise SystemExit(main())
