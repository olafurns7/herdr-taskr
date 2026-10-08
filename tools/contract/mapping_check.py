#!/usr/bin/env python3
"""Check testdata/contract/mapping.tsv against the Rust tests and the Rust surfaces.

Fails on a target missing from `cargo test --workspace -- --list` or the cell
registry, a dropped row without a reason or reviewer, a non-dropped row without
a surface, an unknown surface, or a CLI command, daemon flag or RPC route that
no non-dropped row covers, a duplicate (go_test, go_subtest), an empty family
or an unknown class. todo rows are counted per family; with
TASKR_MAPPING_STRICT=1 they fail, and so does any other row whose reviewed_by
is empty or pending (the S5 gate). Needs no Go toolchain.

Targets: cargo:<test binary>:<test path>, with the binary as cargo names it
without its hash (taskr, taskr_core, golden, ...), or cell:<stem> for
crates/taskr/tests/<stem>.py, or cell:run.py#<family> for a cases.json family.
"""
import collections, json, os, pathlib, re, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
COLUMNS = 'go_test go_subtest go_file family class disposition target surface reason reviewed_by'.split()
DISPOSITIONS = {'blackbox', 'rust-unit', 'golden', 'shell', 'dropped', 'todo'}
CLASSES = {'a', 'b', 'b-conv', 'c', 'rust'}  # rust only on a go_test '-' row
BARE = {'spool', 'migration'}
PREFIXED = {'env', 'install'}


def usage_surfaces():
    """CLI commands, daemon flags and RPC routes, from the Rust sources."""
    cmds, flags = set(), []
    for line in (ROOT / 'crates/taskr/src/read/usage.txt').read_text().splitlines()[1:]:
        label, _, rest = ('', '', line) if line.startswith(' ') else line.partition(':')
        if label in ('outcomes', 'format', 'client'):
            continue
        for seg in re.sub(r'\s{3,}\(.*', '', rest).split(' | '):
            words = seg.split()
            if not words or not re.fullmatch(r'[a-z]+', words[0]):
                continue
            sub = words[0] in ('doc', 'spool') and len(words) > 1
            cmds.add(f'{words[0]}-{words[1]}' if sub else words[0])
            if words[0] == 'daemon':
                flags = re.findall(r'--[a-z]+', seg)
    routes = sorted(set(re.findall(r'path\(\) [!=]= "(/api/[a-z]+)"', (ROOT / 'crates/taskr/src/hub/mod.rs').read_text())))
    return cmds, flags, routes


def cargo_tests():
    cargo = os.environ.get('CARGO', 'cargo')
    # Merged streams keep each "Running" line (stderr) before its list (stdout).
    # Keep default-only tests and include contract-only tests.
    out = '\n'.join(subprocess.run([cargo, 'test', '--workspace', *features, '--', '--list'],
                                  cwd=ROOT, check=True, stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT, text=True).stdout
                    for features in ([], ['--features', 'contract']))
    names, binary = set(), None
    for line in out.splitlines():
        m = re.search(r'Running .*\(.*/deps/([A-Za-z0-9_]+)-[0-9a-f]+(?:\.exe)?\)', line)
        if m:
            binary = m.group(1)
        elif line.endswith(': test') and binary:
            names.add(f'cargo:{binary}:{line[:-len(": test")]}')
    return names


def cells():
    names = {f'cell:{p.stem}' for p in (ROOT / 'crates/taskr/tests').glob('*.py')}
    cases = json.loads((ROOT / 'testdata/contract/cases.json').read_text())
    return names | {f'cell:run.py#{c["family"]}' for c in cases}


def main():
    strict = os.environ.get('TASKR_MAPPING_STRICT') == '1'
    lines = (ROOT / 'testdata/contract/mapping.tsv').read_text().splitlines()
    errors = []
    if lines[0].split('\t') != COLUMNS:
        sys.exit(f'mapping.tsv header must be: {" ".join(COLUMNS)}')
    rows = []
    for i, l in enumerate(lines[1:], 2):
        if l.count('\t') != len(COLUMNS) - 1:
            errors.append(f'line {i}: want {len(COLUMNS)} columns')
        else:
            rows.append(dict(zip(COLUMNS, l.split('\t'))) | {'line': i})
    cmds, flags, routes = usage_surfaces()
    known = {f'cmd:{c}' for c in cmds} | {f'daemon:{f}' for f in flags} | {f'rpc:{r}' for r in routes}
    targets = cargo_tests() | cells() if any(r.get('target') for r in rows) else set()
    covered, todo, keys = set(), collections.Counter(), {}
    for r in rows:
        where = f'line {r["line"]} {r["go_test"]} {r["go_subtest"]}'.rstrip()
        key = (r['go_test'], r['go_subtest'])
        if r['go_test'] != '-' and key in keys:
            errors.append(f'{where}: duplicate of line {keys[key]}')
        keys.setdefault(key, r['line'])
        if not r['family']:
            errors.append(f'{where}: empty family')
        if r['class'] not in CLASSES or (r['class'] == 'rust') != (r['go_test'] == '-'):
            errors.append(f'{where}: class {r["class"]!r} (rust only on a go_test "-" row)')
        d = r['disposition']
        if d not in DISPOSITIONS:
            errors.append(f'{where}: disposition {d!r}')
            continue
        if strict and d != 'todo' and r['reviewed_by'] in ('', 'pending'):
            errors.append(f'{where}: TASKR_MAPPING_STRICT=1: {d} row without a reviewer (reviewed_by {r["reviewed_by"]!r})')
        if d == 'dropped':
            if not r['reason'] or not r['reviewed_by']:
                errors.append(f'{where}: dropped without a reason and reviewed_by')
            continue
        surfaces = [s for s in r['surface'].split(';') if s]
        if not surfaces:
            errors.append(f'{where}: no surface')
        for s in surfaces:
            if s not in known and s not in BARE and s.split(':')[0] not in PREFIXED:
                errors.append(f'{where}: unknown surface {s}')
        covered.update(surfaces)
        if d == 'todo':
            todo[r['family']] += 1
            if r['target']:
                errors.append(f'{where}: todo row with a target')
        elif not r['target']:
            errors.append(f'{where}: {d} without a target')
        for t in filter(None, r['target'].split(';')):
            if t not in targets:
                errors.append(f'{where}: target {t} is not in cargo test --list or the cell registry')
    for s in sorted(known - covered):
        errors.append(f'surface {s} has no non-dropped row')
    counts = collections.Counter(r['disposition'] for r in rows)
    print('mapping_check: ' + ', '.join(f'{k} {v}' for k, v in sorted(counts.items())))
    if todo:
        print(f'todo rows: {sum(todo.values())}: ' + ', '.join(f'{f} {n}' for f, n in sorted(todo.items())))
        if strict:
            errors.append(f'TASKR_MAPPING_STRICT=1: {sum(todo.values())} todo rows')
    for e in errors:
        print(e, file=sys.stderr)
    sys.exit(1 if errors else 0)


if __name__ == '__main__':
    main()
