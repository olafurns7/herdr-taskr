#!/usr/bin/env python3
"""Check fixture cloning and logical row/FTS comparison without any taskr binary."""
from pathlib import Path
import sqlite3
import tempfile
import run as contract
import golden

with tempfile.TemporaryDirectory(prefix='taskr-contract-check-') as scratch:
    directory = Path(scratch)
    fixtures = contract.fixtures(directory, None)
    source = fixtures['busy']
    copy = directory / 'copy.db'
    contract.clone(source, copy)
    assert contract.logical(source) == contract.logical(copy)
    with sqlite3.connect(copy) as db:
        db.execute("update tasks set name='different' where id=1")
    assert contract.logical(source) != contract.logical(copy)
    with sqlite3.connect(copy) as db:
        db.execute('create virtual table search_fts using fts5(body)')
        db.execute("insert into search_fts(body) values ('one two three')")
    before = contract.logical(copy)
    assert 'search_fts' in before['tables']
    assert not any(name.startswith('search_fts_') for name in before['tables'])
    with sqlite3.connect(copy) as db:
        db.execute("insert into search_fts(search_fts) values('optimize')")
    assert contract.logical(copy) == before
    with sqlite3.connect(copy) as db:
        db.execute("update search_fts set body='changed' where rowid=1")
    assert contract.logical(copy) != before
    # An empty Rust-only table (schema 2) is invisible; a row in it is not.
    with sqlite3.connect(copy) as db:
        db.execute('create table subscriptions(id integer primary key, target text, fired_at text)')
        db.execute('create index subscriptions_open on subscriptions(target) where fired_at is null')
    empty = contract.logical(copy)
    assert 'subscriptions' not in empty['tables'] and not any('subscriptions' in row[2] for row in empty['schema'])
    with sqlite3.connect(copy) as db:
        db.execute("insert into subscriptions(target) values ('7')")
    assert 'subscriptions' in contract.logical(copy)['tables']
    binary = Path('/usr/bin/python3').resolve()
    golden.session = golden.Session('selfcheck', True, directory, binary, binary)
    assert golden.normalize(str(directory / 'copy.db')) == '<tmp>/copy.db'
    assert golden.normalize('/synthetic/taskr-code/file') == '/synthetic/taskr-code/file'
    golden.session.ports.add(12345)
    assert golden.normalize('http://127.0.0.1:12345/') == 'http://127.0.0.1:<port>/'
    assert golden.normalize('http://127.0.0.1:0/') == 'http://127.0.0.1:0/'
    assert golden.normalize('http://127.0.0.1:7788/') == 'http://127.0.0.1:7788/'
    assert golden.normalize({'pid': 0, 'new_pid': True, 'daemon_pid': '0', 'lock': '0\n'}) == {'pid': 0, 'new_pid': True, 'daemon_pid': '0', 'lock': '0\n'}
    assert golden.normalize('http://[::1]:12345/') == 'http://[::1]:<port>/'
    assert golden.normalize('http://[::1]:54321/') == 'http://[::1]:54321/'
    assert golden.normalize('http://[::1]:0/') == 'http://[::1]:0/'
    assert golden.normalize('http://[::1]:1/') == 'http://[::1]:1/'
    assert golden.normalize('{"pid":123,"count":123}') == '{"pid":"<pid>","count":123}'
    golden.observe('exit and bytes', (6, b'refused\n', b''))
    golden.finish()
    golden.session = golden.Session('selfcheck', False, directory, binary, binary)
    golden.observe('exit and bytes', (6, b'refused\n', b''))
    golden.finish()
    golden.session = golden.Session('selfcheck', False, directory, binary, binary)
    try:
        golden.observe('exit and bytes', (0, b'refused\n', b''))
    except AssertionError:
        pass
    else:
        raise AssertionError('changed exit was accepted')
    golden.session = golden.Session('selfcheck', False, directory, binary, binary)
    try:
        golden.finish()
    except AssertionError:
        pass
    else:
        raise AssertionError('missing observation was accepted')
    golden.session = golden.Session('run', True, directory, Path('/bin/true').resolve(), binary)
    assert golden.normalize(golden.LOCAL_HOST) == golden.LOCAL_HOST
    assert golden.normalize('{"server_host":"'+golden.LOCAL_HOST+'"}') == '{"server_host":"<local-host>"}'
    golden.observe('gzip bytes', (0, b'dev\n', b''))
    golden.finish()
    packed = (directory / 'run.json.gz').read_bytes()
    import gzip,json
    lines = gzip.decompress(packed).splitlines()
    assert len(lines) == len(golden.session.observations)+3
    assert all('provenance' in json.loads(line.rstrip(b',')) for line in lines[2:-1])
    assert golden.session.observations[0]['provenance'] == 'Go oracle'
    golden.session = golden.Session('run', True, directory, binary, binary)
    golden.observe('gzip bytes', (0, b'dev\n', b''))
    golden.finish()
    assert packed == (directory / 'run.json.gz').read_bytes()
    assert golden.session.observations[0]['provenance'] == 'Go oracle'
    golden.session = golden.Session('run', False, directory, binary, binary)
    golden.observe('gzip bytes', (0, b'dev\n', b''))
    golden.finish()
    with sqlite3.connect(copy) as db:
        db.execute("update tasks set name='changed again' where id=1")
    assert golden.digest(contract.logical(copy, normalized=True)) != golden.digest(before)
    golden.session = golden.Session('replay', True, directory, Path('/bin/true').resolve(), binary)
    record = {'v': 1, 'lock': '42\n', 'identity': {'pid': 42, 'uid': 1000, 'start_time': '99'}}
    golden.observe('Go records', record)
    golden.finish()
    producer_bytes = (directory / 'replay.json').read_bytes()
    golden.session = golden.Session('replay', True, directory, binary, binary)
    golden.observe('Go records', {'identity': {'start_time': '99', 'uid': 1000, 'pid': 42}, 'lock': '42\n', 'v': 1})
    golden.finish()
    assert (directory / 'replay.json').read_bytes() == producer_bytes
    golden.session = golden.Session('replay', False, directory, binary, binary)
    golden.observe('Go records', record)
    assert golden.replay('Go records', {'<pid>': 7, '<pid>\n': '7\n', '<uid>': 3, '<proc-start>': '55'}) == {'v': 1, 'lock': '7\n', 'identity': {'pid': 7, 'uid': 3, 'start_time': '55'}}
    golden.finish()
    golden.session = golden.Session('replay', False, directory, binary, binary)
    try:
        golden.observe('Go records', {**record, 'v': True})
    except AssertionError:
        pass
    else:
        raise AssertionError('JSON boolean was accepted as an integer version')
    golden.session = None
print('fixture clone, row mismatch, visible FTS, shadow exclusion: PASS')
