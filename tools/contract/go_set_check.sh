#!/bin/sh
# Devbox only, outside cargo: the Go tests (go test -list) plus the named
# t.Run sites inside them must equal the go_test/go_subtest rows of
# testdata/contract/mapping.tsv, once each, with the same go_file, and with
# family and class from tests.tsv: class b-conv is a tests.tsv b test in the
# adapter's converted lists. --emit prints the Go set instead.
# A t.Run site is named by its first argument's source text; repeats in one
# test get #2, #3. Sites in helper functions are not rows.
set -eu
cd "$(dirname "$0")/../.."
list=$(go test -list '.*' ./...)
files=$(go list -f '{{range .TestGoFiles}}{{.}} {{end}}' .)
exec python3 - "${1:-}" "$files" "$list" <<'EOF'
import collections, re, sys
mode, files, listing = sys.argv[1], sys.argv[2].split(), sys.argv[3]
listed = {l for l in listing.splitlines() if re.fullmatch(r'Test\w+', l)}
FUNC = re.compile(r'^func (\w+)\(')
RUN = re.compile(r'\bt\.Run\((.*?), func\(')
rows, seen = [], set()
for f in sorted(files):
    cur = None
    n = collections.Counter()
    for line in open(f):
        m = FUNC.match(line)
        if m:
            cur = m.group(1) if m.group(1) in listed else None
            if cur:
                seen.add(cur)
                rows.append((cur, '', f))
            n = collections.Counter()
            continue
        if cur:
            for m in RUN.finditer(line):
                expr = ' '.join(m.group(1).split())
                n[expr] += 1
                rows.append((cur, expr + (f'#{n[expr]}' if n[expr] > 1 else ''), f))
if listed - seen:
    sys.exit('tests listed but not found in TestGoFiles: ' + ' '.join(sorted(listed - seen)))
if mode == '--emit':
    for r in rows:
        print('\t'.join(r))
    sys.exit()
tsv = {}
for line in open('testdata/contract/tests.tsv').read().splitlines()[1:]:
    t, family, _, group, _ = line.split('\t')
    tsv[t] = (family, group)
net = open('net_contract_adapter_test.go').read().split('var contractNetConverted')[1].split('\n}\n')[0]
converted = set(re.findall(r'"(Test\w+)"', net + open('daemon_contract_adapter_test.go').read()))
def want(t, s, f):
    family, group = tsv.get(t, ('?', '?'))
    return (t, s, f, family, 'b-conv' if group == 'b' and t in converted else group)
go = collections.Counter(want(*r) for r in rows)
mapped = collections.Counter()
for line in open('testdata/contract/mapping.tsv').read().splitlines()[1:]:
    cols = line.split('\t')
    if cols[0] != '-':
        mapped[tuple(cols[:5])] += 1
missing, extra = sorted(go - mapped), sorted(mapped - go)
for r in missing:
    print('missing or different row: ' + '\t'.join(r))
for r in extra:
    print('row without a matching Go test: ' + '\t'.join(r))
if missing or extra:
    sys.exit(1)
print(f'go_set_check: {len(go)} Go tests and subtest sites match mapping.tsv')
EOF
