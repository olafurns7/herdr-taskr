#!/usr/bin/env python3
"""Check fixture cloning and logical row/FTS comparison without any taskr binary."""
from pathlib import Path
import sqlite3
import tempfile
import run as contract

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
print('fixture clone, row mismatch, visible FTS, shadow exclusion: PASS')
