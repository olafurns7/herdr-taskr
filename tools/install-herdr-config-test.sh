#!/bin/sh
# Extract the sidebar step verbatim: no downloads, daemon or real config writes.
set -eu
cd "$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
mkdir -p .scratch
t=$(mktemp -d "$PWD/.scratch/install-herdr-config.XXXXXX")
mkdir -p "$t/bin" "$t/home/.config/herdr" "$t/radar settings"
printf '%s\n' 'main config untouched' > "$t/home/.config/herdr/config.toml"
cp "$t/home/.config/herdr/config.toml" "$t/main.before"
cat > "$t/bin/herdr" <<'EOF'
#!/bin/sh
set -eu
[ "$*" = 'plugin config-dir hhdebb.herdr-radar' ] || exit 9
printf '%s\n' "$*" >> "$FAKE_HERDR_LOG"
[ "${FAKE_HERDR_FAIL:-0}" = 0 ] || exit 1
printf '%s\n' "$FAKE_RADAR_DIR"
EOF
chmod +x "$t/bin/herdr"
sed -n '/^manual_herdr_sidebar() {/,/^}/p' install.sh > "$t/step.sh"
sed -n '/^configure_herdr_sidebar() (/,/^)/p' install.sh >> "$t/step.sh"
grep -q '^configure_herdr_sidebar()' "$t/step.sh"
cat >> "$t/step.sh" <<'EOF'
fail() { printf '%s\n' "$*" >&2; exit 1; }
EOF
cp "$t/step.sh" "$t/installer-tail.sh"
printf '%s\n' 'configure_herdr_sidebar' >> "$t/step.sh"
# The real installer's optional-step handler and PATH hint, with no download/install.
printf '%s\n' 'DIR=$HOME/continuation-bin' >> "$t/installer-tail.sh"
sed -n '/^if ! configure_herdr_sidebar; then/,$p' install.sh >> "$t/installer-tail.sh"
export FAKE_RADAR_DIR="$t/radar settings" FAKE_HERDR_LOG="$t/herdr.log"
run() {
	HOME="$t/home" PATH="$t/bin:$PATH" TASKR_HERDR_CONFIG=$1 HERDR_ENV=$2 \
		FAKE_HERDR_FAIL=${3:-0} "${TASKR_TEST_SHELL:-sh}" "$t/${4:-step}.sh" > "$t/out" 2>&1
}
manual() {
	grep -qx 'MANUAL: find the herdr-radar settings directory: herdr plugin config-dir hhdebb.herdr-radar' "$t/out"
	for key in 'parent_token = "taskr_parent"' 'parent_label_token = "taskr_campaign"' \
		'attention_token = "taskr_owner_ask"' 'space_owner = false'; do
		grep -qx "$key" "$t/out"
	done
}
run '' 1
manual
[ ! -e "$t/herdr.log" ] && [ ! -e "$FAKE_RADAR_DIR/config.toml" ]
run 1 ''
manual
[ ! -e "$t/herdr.log" ] && [ ! -e "$FAKE_RADAR_DIR/config.toml" ]
run 1 1 1
manual
[ ! -e "$FAKE_RADAR_DIR/config.toml" ]
printf '%s\n' 'ok opt-in, Herdr and config-dir success required'

run 1 1
cat > "$t/want" <<'EOF'
parent_token = "taskr_parent"
parent_label_token = "taskr_campaign"
attention_token = "taskr_owner_ask"
space_owner = false
EOF
cmp "$t/want" "$FAKE_RADAR_DIR/config.toml"
printf '%s\n' 'ok new config gets all four keys'
cat > "$FAKE_RADAR_DIR/config.toml" <<'EOF'
# Keep root values, including quoted keys and comments.
"parent_token" = "custom-parent"
'space_owner' = true # custom
other = 7
[colors]
attention_token = "nested-only"
EOF
cat > "$t/want" <<'EOF'
# Keep root values, including quoted keys and comments.
"parent_token" = "custom-parent"
'space_owner' = true # custom
other = 7
parent_label_token = "taskr_campaign"
attention_token = "taskr_owner_ask"
[colors]
attention_token = "nested-only"
EOF
chmod 640 "$FAKE_RADAR_DIR/config.toml"
run 1 1
cmp "$t/want" "$FAKE_RADAR_DIR/config.toml"
[ "$(ls -l "$FAKE_RADAR_DIR/config.toml" | cut -c1-10)" = -rw-r----- ]
printf '%s\n' 'ok existing values preserved; missing keys precede tables'
run 1 1
cmp "$t/want" "$FAKE_RADAR_DIR/config.toml"
cmp "$t/main.before" "$t/home/.config/herdr/config.toml"
printf '%s\n' 'ok idempotent; main Herdr config untouched'
cat > "$FAKE_RADAR_DIR/config.toml" <<'EOF'
description = """
[not_a_table]
"""
parent_token = "custom-parent"
EOF
cp "$FAKE_RADAR_DIR/config.toml" "$t/complex.before"
if run 1 1; then printf '%s\n' 'FAIL accepted complex TOML'; exit 1; fi
cmp "$t/complex.before" "$FAKE_RADAR_DIR/config.toml"
grep -q 'configure complex TOML manually' "$t/out"
printf '%s\n' 'ok complex TOML left unchanged for manual setup'
for complex in array dotted; do
	case $complex in
	array) printf 'arr = [\n  [1, 2],\n]\n' > "$FAKE_RADAR_DIR/config.toml" ;;
	dotted) printf 'parent_token.x = 1\n' > "$FAKE_RADAR_DIR/config.toml" ;;
	esac
	cp "$FAKE_RADAR_DIR/config.toml" "$t/complex.before"
	if run 1 1; then printf 'FAIL accepted complex %s\n' "$complex"; exit 1; fi
	cmp "$t/complex.before" "$FAKE_RADAR_DIR/config.toml"
	grep -q 'configure complex TOML manually' "$t/out"
	printf 'ok complex %s refused without changing config\n' "$complex"
done
cp "$t/want" "$FAKE_RADAR_DIR/config.toml"
mv "$FAKE_RADAR_DIR/config.toml" "$t/symlink-target"
ln -s "$t/symlink-target" "$FAKE_RADAR_DIR/config.toml"
if run 1 1; then printf '%s\n' 'FAIL accepted a symlink'; exit 1; fi
cmp "$t/want" "$t/symlink-target"
grep -q 'is a symlink' "$t/out"
run 1 1 0 installer-tail
manual
grep -q 'is a symlink' "$t/out"
grep -q 'install.sh: add .*continuation-bin to PATH' "$t/out"
cmp "$t/want" "$t/symlink-target"
printf '%s\n' 'ok symlink refusal prints manual steps and installer PATH hint'
printf 'ok symlink refused; fixtures retained at %s\n' "$t"
