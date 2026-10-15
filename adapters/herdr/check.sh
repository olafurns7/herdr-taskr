#!/bin/sh
# Fake-only launcher checks: no Herdr socket, native agent or real ledger is used.
set -eu
repo=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd -P)
mkdir -p "$repo/.scratch"
scratch=$(mktemp -d "$repo/.scratch/worker-launcher-test.XXXXXX")
# Keep evidence in .scratch; do not remove a path assembled by the harness.
mkdir "$scratch/bin" "$scratch/home" "$scratch/work dir"
export FAKE_ROOT=$scratch HOME=$scratch/home
export PATH="$scratch/bin:$PATH"
unset TASKR_TASK TASKR_LAUNCH TASKR_BIN TASKR_DB
launcher=$repo/adapters/herdr/taskr-herdr-launch
printf 'HERDR-BRIEF role=implementer\n' > "$scratch/brief with spaces.md"

cat > "$scratch/common" <<'EOF'
set -eu
jq -cn --args '$ARGS.positional' -- "$@" >> "$FAKE_ROOT/argv"
fail() { [ "${FAIL_AT:-}" != "$1" ] || exit "${FAIL_CODE:-9}"; }
EOF
cat > "$scratch/bin/taskr" <<'EOF'
#!/bin/sh
set -- taskr "$@"
. "$FAKE_ROOT/common"
shift
[ -z "${TASKR_FORMAT:-}" ] || exit 10
fail "taskr-$1"
case $1 in
new) printf '%s\n' "${NEW_OUTPUT:-n1 42}" ;;
launch) printf '%s\n' "${LAUNCH_OUTPUT:-l1 {\"launch_id\":73}}" ;;
prompt) [ -f "$FAKE_ROOT/agent-env" ] || exit 11 ;;
*) exit 12 ;;
esac
EOF
cat > "$scratch/bin/herdr" <<'EOF'
#!/bin/sh
set -- herdr "$@"
. "$FAKE_ROOT/common"
shift
fail "$1-$2"
case "$1 $2" in
'pane get')
	if [ -n "${PANE_OUTPUT:-}" ]; then printf '%s\n' "$PANE_OUTPUT"; else
		jq -cn --arg cwd "$FAKE_ROOT/work dir" '{id:"cli:pane:get",result:{type:"pane_info",pane:{workspace_id:"w1",tab_id:"w1:t2",cwd:$cwd,tokens:{cwd:"ignored nested value"}}}}'
	fi ;;
'pane process-info')
	if [ -n "${PROCESS_OUTPUT:-}" ]; then printf '%s\n' "$PROCESS_OUTPUT"; else
		printf '%s\n' '{"id":"cli:pane:process_info","result":{"process_info":{"shell_pid":22,"foreground_process_group_id":22}}}'
	fi ;;
'pane send-keys') fail "key-$4" ;;
'pane run')
	case $4 in
	' export '*) fail export; printf '%s\n' "$4" > "$FAKE_ROOT/env" ;;
	*) fail env-check; sh -c '. "$FAKE_ROOT/env"; eval "$1"' sh "$4" > "$FAKE_ROOT/shell-output" ;;
	esac ;;
'pane wait-output')
	grep -F -x -- "$5" "$FAKE_ROOT/shell-output" >/dev/null ;;
'agent start')
	[ -z "${START_DIALOG:-}" ] || exit 1
	[ "$4" = --kind ] && [ "$6" = --pane ] && [ "$8" = --timeout ] && [ "${10}" = -- ] || exit 13
	kind=$5
	shift 10
	sh -c '. "$FAKE_ROOT/env"; exec "$@"' sh "$kind" "$@" ;;
'pane read') printf '%s\n' "${DIALOG_TEXT:-Loading development channels}" ;;
'agent wait')
	sh -c '. "$FAKE_ROOT/env"; exec claude' ;;
*) exit 14 ;;
esac
EOF
cat > "$scratch/bin/claude" <<'EOF'
#!/bin/sh
set -- agent "$@"
. "$FAKE_ROOT/common"
shift
[ "$TASKR_TASK/$TASKR_LAUNCH" = 42/73 ] || exit 15
printf '%s/%s\n' "$TASKR_TASK" "$TASKR_LAUNCH" > "$FAKE_ROOT/agent-env"
fail agent
EOF
cp "$scratch/bin/claude" "$scratch/bin/codex"
cp "$scratch/bin/claude" "$scratch/bin/pi"
cp "$scratch/bin/claude" "$scratch/bin/opencode"
chmod +x "$scratch/bin/"*

count=0
run() {
	: > "$scratch/argv"
	rm -f "$scratch/env" "$scratch/shell-output" "$scratch/agent-env"
	if TASKR_FORMAT=json sh "$launcher" --name 'worker-any' --role implementer --parent 7 \
		--model chosen-model --effort chosen-effort --report "$scratch/report.md" "$@" \
		> "$scratch/out" 2> "$scratch/err"; then code=0; else code=$?; fi
}
assert() {
	if ! eval "$1"; then cat "$scratch/out" "$scratch/err" "$scratch/argv"; printf 'FAIL: %s\n' "$2"; exit 1; fi
	count=$((count + 1))
}
calls() { jq -es "$1" "$scratch/argv" >/dev/null; }
plain() { run claude w1:p2 "$scratch/brief with spaces.md" -- --model chosen-model --settings 'a b;$(literal)'; }
plain
assert '[ "$code" -eq 0 ]' 'plain launch succeeds'
assert 'calls '\''map(.[0:2]) == [["herdr","pane"],["herdr","pane"],["herdr","pane"],["taskr","new"],["taskr","launch"],["herdr","pane"],["herdr","pane"],["herdr","pane"],["herdr","agent"],["agent","--model"],["taskr","prompt"]]'\''' 'exact launch sequence'
assert 'calls '\''.[3] == ["taskr","new","worker-any","--parent","7","--role","implementer","--workspace","w1","--tab","w1:t2","--pane","w1:p2","--cwd",env.FAKE_ROOT + "/work dir","--brief",env.FAKE_ROOT + "/brief with spaces.md","--report",env.FAKE_ROOT + "/report.md"]'\''' 'registration argv preserves paths'
assert 'calls '\''.[4] == ["taskr","launch","42","--provider","claude","--model","chosen-model","--effort","chosen-effort"]'\''' 'launch metadata'
assert 'calls '\''.[9] == ["agent","--model","chosen-model","--settings","a b;$(literal)"]'\''' 'native argv survives literally'
assert '[ "$(cat "$scratch/agent-env")" = 42/73 ]' 'agent inherits pane exports'

for kind in codex pi opencode; do
	run --provider custom "$kind" w1:p2 "$scratch/brief with spaces.md" --
	assert '[ "$code" -eq 0 ] && calls '\''.[4][4] == "custom"'\''' "$kind and explicit provider"
done
run --push claude w1:p2 "$scratch/brief with spaces.md" -- --model chosen-model
assert '[ "$code" -eq 0 ]' 'push launch succeeds'
config=$(jq -rs 'map(select(.[0:2] == ["herdr","agent"]))[0] | .[index("--mcp-config") + 1]' "$scratch/argv")
assert '[ -f "$config" ] && [ "$(LC_ALL=C ls -ld "$config" | cut -c1-10)" = -rw------- ]' 'push config mode 0600'
assert '[ "$(LC_ALL=C ls -ld "${config%/*}" | cut -c1-10)" = drwx------ ]' 'private directory mode 0700'
assert 'jq -e '\''. == {mcpServers:{taskr:{command:"taskr",args:["mcp","--channel"]}}}'\'' "$config" >/dev/null' 'push config content'
assert 'calls '\''map(select(.[0:2] == ["herdr","agent"]))[0][-2:] == ["--dangerously-load-development-channels","server:taskr"]'\''' 'push native channel flag'
first_config=$config
run --push claude w1:p2 "$scratch/brief with spaces.md" --
assert '[ "$code" -eq 0 ] && [ -f "$first_config" ] && ! grep -F "$first_config" "$scratch/out"' 'push config uses a fresh directory'

START_DIALOG=1; export START_DIALOG
run --push claude w1:p2 "$scratch/brief with spaces.md" --
assert '[ "$code" -eq 0 ] && calls '\''any(.[]; . == ["herdr","pane","send-keys","w1:p2","Enter"]) and any(.[]; .[0:3] == ["herdr","agent","wait"])'\''' 'confirmed development dialog then idle'
DIALOG_TEXT='some unrelated error'; export DIALOG_TEXT
run --push claude w1:p2 "$scratch/brief with spaces.md" --
assert '[ "$code" -ne 0 ] && ! calls '\''any(.[]; .[-1] == "Enter" or .[0:2] == ["taskr","prompt"])'\''' 'unrelated startup failure is not confirmed'
unset START_DIALOG DIALOG_TEXT

START_DIALOG=1; export START_DIALOG
for step in pane-read key-Enter agent-wait; do
	FAIL_AT=$step; export FAIL_AT
	run --push claude w1:p2 "$scratch/brief with spaces.md" --
	assert '[ "$code" -ne 0 ] && ! calls '\''any(.[]; .[0:2] == ["taskr","prompt"])'\''' "no brief after $step failure"
done
unset START_DIALOG FAIL_AT

for step in pane-get pane-process-info key-ctrl+c taskr-new taskr-launch export env-check pane-wait-output agent-start taskr-prompt; do
	FAIL_AT=$step; export FAIL_AT
	plain
	assert '[ "$code" -ne 0 ] && grep -q "taskr-herdr-launch:" "$scratch/err"' "stops on $step failure"
	if [ "$step" != taskr-prompt ]; then
		assert '! calls '\''any(.[]; .[0:2] == ["taskr","prompt"])'\''' "no brief after $step failure"
	fi
done
FAIL_AT=taskr-launch FAIL_CODE=6; export FAIL_AT FAIL_CODE
plain
assert '[ "$code" -eq 6 ]' 'taskr rejection exit 6 is preserved'
unset FAIL_AT FAIL_CODE

# A failing config creator must stop before agent startup, leaving project config alone.
cat > "$scratch/bin/mktemp" <<'EOF'
#!/bin/sh
exit 9
EOF
chmod +x "$scratch/bin/mktemp"
printf 'project config stays\n' > "$scratch/work dir/.mcp.json"
run --push claude w1:p2 "$scratch/brief with spaces.md" --
assert '[ "$code" -ne 0 ] && ! calls '\''any(.[]; .[0:2] == ["herdr","agent"])'\''' 'config creation failure prevents agent start'
assert '[ "$(cat "$scratch/work dir/.mcp.json")" = "project config stays" ]' 'project config is untouched'
rm "$scratch/bin/mktemp"

mkdir "$scratch/collision"
printf 'must not be overwritten\n' > "$scratch/sentinel"
ln -s "$scratch/sentinel" "$scratch/collision/mcp.json"
cat > "$scratch/bin/mktemp" <<'EOF'
#!/bin/sh
printf '%s/collision\n' "$FAKE_ROOT"
EOF
chmod +x "$scratch/bin/mktemp"
run --push claude w1:p2 "$scratch/brief with spaces.md" --
assert '[ "$code" -ne 0 ] && ! calls '\''any(.[]; .[0:2] == ["herdr","agent"])'\''' 'existing config symlink is refused'
assert '[ "$(cat "$scratch/sentinel")" = "must not be overwritten" ]' 'exclusive config creation does not follow a symlink'
rm "$scratch/bin/mktemp"

cat > "$scratch/bin/chmod" <<'EOF'
#!/bin/sh
exit 9
EOF
chmod +x "$scratch/bin/chmod"
run --push claude w1:p2 "$scratch/brief with spaces.md" --
assert '[ "$code" -ne 0 ] && ! calls '\''any(.[]; .[0:2] == ["herdr","agent"])'\''' 'private directory protection failure prevents startup'
rm "$scratch/bin/chmod"

for output in 'n1 42;echo bad' 'n1 0' 'n1 42
n1 43' 'qd1 queued'; do
	NEW_OUTPUT=$output; export NEW_OUTPUT; plain
	assert '[ "$code" -ne 0 ] && ! calls '\''any(.[]; .[0:2] == ["taskr","launch"])'\''' 'malformed or queued task identity stops'
done
unset NEW_OUTPUT
for output in 'l1 {"launch_id":"73;bad"}' 'l1 {"launch_id":0}' 'l1 {"launch_id":1.5}' 'l1 {}' 'l1 nope'; do
	LAUNCH_OUTPUT=$output; export LAUNCH_OUTPUT; plain
	assert '[ "$code" -ne 0 ] && ! calls '\''any(.[]; .[0:3] == ["herdr","pane","run"])'\''' 'invalid launch identity never reaches shell'
done
unset LAUNCH_OUTPUT
PROCESS_OUTPUT='{"id":"cli:pane:process_info","result":{"process_info":{"shell_pid":22,"foreground_process_group_id":23}}}'; export PROCESS_OUTPUT
plain
assert '[ "$code" -ne 0 ] && grep -q "pane is not at its shell prompt" "$scratch/err" && ! calls '\''any(.[]; .[0] == "taskr")'\''' 'busy pane refused before registration'
unset PROCESS_OUTPUT
PANE_OUTPUT='{"id":"cli:pane:get","cwd":"/","result":{"type":"pane_info","pane":{"workspace_id":"w1","tab_id":"w1:t2","tokens":{"cwd":"/"}}}}'; export PANE_OUTPUT
plain
assert '[ "$code" -ne 0 ] && grep -q "pane cwd is missing or invalid" "$scratch/err" && ! calls '\''any(.[]; .[0] == "taskr")'\''' 'nested or top-level cwd cannot substitute for pane cwd'
unset PANE_OUTPUT
for args in 'pi' 'codex'; do
	run --push "$args" w1:p2 "$scratch/brief with spaces.md" --
	assert '[ "$code" -eq 2 ] && [ ! -s "$scratch/argv" ]' 'unsupported push fails before any call'
done
run claude w1:p2 "$scratch/missing" --
assert '[ "$code" -eq 2 ] && [ ! -s "$scratch/argv" ]' 'missing brief refused'
run --role orchestrator claude w1:p2 "$scratch/brief with spaces.md" --
assert '[ "$code" -eq 2 ] && [ ! -s "$scratch/argv" ]' 'root role refused'
run --parent not-an-id claude w1:p2 "$scratch/brief with spaces.md" --
assert '[ "$code" -eq 2 ] && [ ! -s "$scratch/argv" ]' 'invalid parent refused'
run --report relative.md claude w1:p2 "$scratch/brief with spaces.md" --
assert '[ "$code" -eq 2 ] && [ ! -s "$scratch/argv" ]' 'relative report refused'
run --unknown value claude w1:p2 "$scratch/brief with spaces.md" --
assert '[ "$code" -eq 2 ] && [ ! -s "$scratch/argv" ]' 'unknown option refused'
run claude w1:p2 "$scratch/brief with spaces.md"
assert '[ "$code" -eq 2 ] && [ ! -s "$scratch/argv" ]' 'missing native argument separator refused'
mv "$scratch/bin/claude" "$scratch/bin/missing-claude"
PATH="$scratch/bin:/usr/bin:/bin" plain
assert '[ "$code" -eq 2 ] && [ ! -s "$scratch/argv" ]' 'missing agent refused before registration'
mv "$scratch/bin/missing-claude" "$scratch/bin/claude"
printf 'worker-launcher: %s checks passed; evidence: %s\n' "$count" "$scratch"
