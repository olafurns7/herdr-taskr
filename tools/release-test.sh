#!/bin/sh
# Refusal tests for release.sh: test-only hooks in an imported darwin pair, the
# adapter summary parser, and --promote never moving "latest" back.
# Usage: sh tools/release-test.sh (needs release.sh's Linux build tools, for its preflight).
# TASKR_RELEASE_SSH and TASKR_RELEASE_GH are fakes: no test reaches a real host or GitHub.
set -eu
cd "$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
t=$(mktemp -d /tmp/taskr-release-test.XXXXXX)
trap 'case "$t" in /tmp/taskr-release-test.??????) rm -rf -- "$t" ;; *) echo "release-test: refusing cleanup of $t" >&2 ;; esac' 0
pass=0
ok() { pass=$((pass + 1)); printf 'ok %s\n' "$1"; }
no() { printf 'FAIL %s\n' "$1"; exit 1; }

# The fake remote fails every call; the fake gh answers only the two promote calls.
printf '#!/bin/sh\necho "fake-ssh: unexpected: $*" >&2\nexit 9\n' > "$t/ssh"
cat > "$t/gh" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_GH_LOG"
case $* in
'release view --repo olafurns7/herdr-taskr --json tagName --jq .tagName') printf '%s\n' "$FAKE_LATEST" ;;
'release edit '*) ;;
*) echo "fake-gh: unexpected: $*" >&2; exit 9 ;;
esac
EOF
chmod +x "$t/ssh" "$t/gh"
export TASKR_RELEASE_SSH="$t/ssh" TASKR_RELEASE_GH="$t/gh" FAKE_GH_LOG="$t/gh.log" FAKE_LATEST=v0.0.0

# A Mach-O-headed stub for one arch (cputype bytes as octal) carrying the given text.
macho() {
	# shellcheck disable=SC2059 # $2 is octal escapes for the format
	{ printf '\317\372\355\376'"$2"'\002\000\000\000'; printf '\000%.0s' 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16; printf '\n%s\n' "$3"; } > "$1"
}
pair() { # DIR ARM64_TEXT
	mkdir -p "$1"
	macho "$1/taskr-rust-aarch64-apple-darwin" '\014\000\000\001\000\000\000\000' "$2"
	macho "$1/taskr-rust-x86_64-apple-darwin" '\007\000\000\001\003\000\000\000' 'stub v0.17.0'
	(cd "$1" && sha256sum taskr-rust-* > SHA256SUMS)
}

# A checksummed imported pair with a test-only marker is refused before the gate.
for marker in TASKR_FROZEN_NOW TASKR_CONTRACT_FROZEN_NOW --contract-seed; do
	pair "$t/import$marker" "stub v0.17.0 $marker"
	if ./release.sh v0.17.0 --dry-run --darwin-from "$t/import$marker" > "$t/out" 2>&1; then
		no "imported pair with $marker accepted"
	fi
	grep -q 'contains a test-only hook' "$t/out" || { cat "$t/out"; no "imported pair with $marker: wrong refusal"; }
	ok "refuses an imported pair with $marker"
done

# The gate's summary parser, extracted verbatim from release.sh.
sed -n "/python3 - \"\$work\/adapter-summary.json\" <<'EOF'/,/^EOF\$/p" release.sh | sed '1d;$d' > "$t/parser.py"
grep -q 'pass") != 196' "$t/parser.py" || no 'summary parser not found in release.sh'
summary() { # NAME WANT(accept|refuse) JSON
	printf '%s\n' "$3" > "$t/summary.json"
	if python3 "$t/parser.py" "$t/summary.json" > "$t/out" 2>&1; then got=accept; else got=refuse; fi
	[ "$got" = "$2" ] || { cat "$t/out"; no "summary $1: $got"; }
	ok "summary $1: $2"
}
summary 'filtered to one family' refuse '{"read:version":{"pass":1,"skipped-family":195}}'
summary 'all passing but filtered' refuse '{"read":{"pass":196,"skipped-family":3,"skipped-internals":284}}'
summary 'other skip' refuse '{"read":{"pass":195,"skipped-other":1,"skipped-internals":287}}'
summary 'mismatch' refuse '{"read":{"pass":195,"mismatch":1}}'
summary 'not implemented' refuse '{"read":{"pass":195,"not-implemented":1}}'
summary 'short pass count' refuse '{"read":{"pass":195,"skipped-internals":287}}'
summary 'empty' refuse '{}'
summary 'complete' accept '{"read":{"pass":150,"skipped-internals":200},"write":{"pass":46,"skipped-internals":87}}'

# --promote reads the latest release and refuses an older tag.
promote() { # TAG LATEST WANT(edit|refuse)
	: > "$t/gh.log"
	if FAKE_LATEST=$2 ./release.sh "$1" --promote > "$t/out" 2>&1; then got=0; else got=$?; fi
	if [ "$3" = edit ]; then
		if [ "$got" -ne 0 ] || ! grep -qx "release edit $1 --repo olafurns7/herdr-taskr --prerelease=false --latest" "$t/gh.log"; then
			cat "$t/out" "$t/gh.log"
			no "promote $1 over $2 should edit"
		fi
	elif [ "$got" -eq 0 ] || grep -q 'release edit' "$t/gh.log" || ! grep -q 'older than the latest release' "$t/out"; then
		cat "$t/out" "$t/gh.log"
		no "promote $1 over $2 should refuse"
	fi
	ok "promote $1 over latest $2: $3"
}
promote v0.16.0 v0.16.1 refuse
promote v0.9.9 v0.16.1 refuse
promote v0.17.0 v0.16.1 edit
promote v0.16.10 v0.16.9 edit
promote v0.16.1 v0.16.1 edit
printf 'release-test: all %s passed\n' "$pass"
