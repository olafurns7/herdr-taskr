#!/bin/sh
# push-canary.sh: a scratch-only ledger for push adapter trials (docs/push.md).
#
#   init DIR          create DIR (it must not exist) with a ledger: root 1 and a launched
#                     sender lane 2; print the environment prefix for the client
#   fire DIR N        fire event N into root 1's inbox: the sender's `ready "canary event N"`
#   capture DIR NAME  write inbox, delivery ls, consumer show, status and log of root 1
#                     into the new directory DIR/evidence/NAME
#   selftest          init, fire and capture in a new temporary directory, no client
#
# Every taskr call runs with a cleared environment and this directory's HOME, TASKR_DB
# and TASKR_TMP_BASE, so an inherited identity or ledger never applies. The taskr binary
# is TASKR_CANARY_BIN (an absolute path) or taskr on PATH. Each command first checks the
# marker init wrote and the directory's layout, and refuses symlinks, a multiply-linked
# ledger and missing state (never recreating it). The marker protects against mistakes,
# not against another process of the same user: nothing else may change DIR meanwhile.
# It starts no client and edits no configuration.
set -u
MARK=.push-canary

usage() {
	echo "usage: push-canary.sh init DIR | fire DIR N | capture DIR NAME | selftest" >&2
	exit 2
}
die() {
	echo "push-canary: refused: $*" >&2
	exit 1
}

pin() {
	TASKR=${TASKR_CANARY_BIN:-$(command -v taskr || true)}
	case $TASKR in
	/*) ;;
	*) die "TASKR_CANARY_BIN must be an absolute path to taskr (got '$TASKR')" ;;
	esac
	[ -f "$TASKR" ] && [ -x "$TASKR" ] || die "$TASKR is not an executable file"
}

# DIR as an absolute path under its parent's physical path; DIR itself is not followed.
place() {
	base=$(basename -- "$1")
	case $base in
	'' | . | .. | /) die "'$1' is not a directory name" ;;
	esac
	parent=$(cd -P -- "$(dirname -- "$1")" 2>/dev/null && pwd -P) || die "no parent directory for '$1'"
	DIR=$parent/$base
}

links() {
	ls -ld -- "$1" | awk '{print $2}'
}

# The marker, the layout and the state of a directory init created.
owned() {
	place "$1"
	[ -L "$DIR" ] && die "$DIR is a symlink"
	[ -d "$DIR" ] || die "$DIR is not a directory"
	[ -L "$DIR/$MARK" ] && die "the canary marker $DIR/$MARK is a symlink"
	[ -f "$DIR/$MARK" ] || die "no canary marker in $DIR: not a directory this script created"
	[ "$(cat -- "$DIR/$MARK")" = "taskr push canary $DIR" ] ||
		die "the marker does not name $DIR (copied or moved)"
	for d in home evidence tmp; do
		[ -L "$DIR/$d" ] && die "$DIR/$d is a symlink"
		[ -d "$DIR/$d" ] || die "$DIR/$d is missing"
	done
	for f in ledger.db ledger.db-wal ledger.db-shm sender; do
		[ -L "$DIR/$f" ] && die "$DIR/$f is a symlink"
	done
	for f in ledger.db sender; do
		[ -f "$DIR/$f" ] || die "$DIR/$f is missing; refusing to recreate state"
		[ "$(links "$DIR/$f")" = 1 ] || die "$DIR/$f has another hard link"
	done
	# SQLite creates these, so absence is fine; an existing one is written in place.
	for f in ledger.db-wal ledger.db-shm; do
		[ -e "$DIR/$f" ] || continue
		[ -f "$DIR/$f" ] || die "$DIR/$f is not a regular file"
		[ "$(links "$DIR/$f")" = 1 ] || die "$DIR/$f has another hard link"
	done
}

# taskr on this directory's ledger only; extra NAME=VALUE words come first.
t() {
	env -i PATH=/usr/bin:/bin LC_ALL=C HOME="$DIR/home" TASKR_DB="$DIR/ledger.db" \
		TASKR_TMP_BASE="$DIR/tmp" "$@"
}
field() {
	sed -n "s/.*\"$1\":\([0-9][0-9]*\).*/\1/p"
}

init() {
	[ $# -eq 1 ] || usage
	pin
	place "$1"
	if [ -e "$DIR" ] || [ -L "$DIR" ]; then
		die "$DIR already exists: init only creates a new directory"
	fi
	(umask 077 && mkdir -- "$DIR") || die "cannot create $DIR"
	(umask 077 && mkdir -- "$DIR/home" "$DIR/evidence" "$DIR/tmp") || die "cannot lay out $DIR"
	printf 'taskr push canary %s\n' "$DIR" >"$DIR/$MARK"
	root=$(t "$TASKR" --json new canary-root --role orchestrator 2>/dev/null | field task_id)
	sender=$(t "$TASKR" --json new canary-sender --role implementer --parent 1 2>/dev/null | field task_id)
	launch=$(t "$TASKR" --json launch 2 --provider canary --model synthetic --effort low | field launch_id)
	[ "$root $sender" = "1 2" ] && [ -n "$launch" ] || die "the ledger in $DIR did not start empty"
	printf '%s %s\n' "$sender" "$launch" >"$DIR/sender"
	echo "canary ledger $DIR: root 1 (--as 1 --session 0), sender task 2 launch $launch"
	echo "taskr prefix: env -u TASKR_TASK -u TASKR_LAUNCH HOME=$DIR/home TASKR_DB=$DIR/ledger.db TASKR_TMP_BASE=$DIR/tmp"
}

fire() {
	[ $# -eq 2 ] || usage
	case $2 in '' | *[!0-9]*) usage ;; esac
	pin
	owned "$1"
	read -r task launch <"$DIR/sender"
	case "$task$launch" in '' | *[!0-9]*) die "$DIR/sender is not two ids" ;; esac
	t TASKR_TASK="$task" TASKR_LAUNCH="$launch" "$TASKR" ready "canary event $2"
}

capture() {
	[ $# -eq 2 ] || usage
	pin
	owned "$1"
	case $2 in
	'' | .* | *[!A-Za-z0-9._-]*) die "NAME must be a new plain name: letters, digits, . _ -, not starting with ." ;;
	esac
	out=$DIR/evidence/$2
	if [ -e "$out" ] || [ -L "$out" ]; then
		die "$out exists"
	fi
	(umask 077 && mkdir -- "$out") || die "cannot create $out"
	for step in "inbox:inbox --as 1" "delivery:delivery ls --as 1" "consumer:consumer show --as 1" \
		"status:status --tree 1" "log:log 1 --tree --limit 0"; do
		name=${step%%:*}
		# shellcheck disable=SC2086 # the step's words are fixed above
		t "$TASKR" ${step#*:} >"$out/$name.txt" 2>&1
		echo "exit $?" >>"$out/$name.txt"
	done
	echo "captured $out"
}

selftest() {
	[ $# -eq 0 ] || usage
	pin
	base=$(mktemp -d "${TMPDIR:-/tmp}/push-canary.XXXXXX") || die "mktemp failed"
	d=$base/canary
	check() {
		"$@" || {
			echo "push-canary: selftest failed: $*; scratch left in $base" >&2
			exit 1
		}
	}
	check sh "$0" init "$d" >"$base/init.txt"
	check sh "$0" fire "$d" 1 >"$base/fire.txt"
	check sh "$0" capture "$d" selftest >"$base/capture.txt"
	check grep -q 'canary event 1' "$d/evidence/selftest/inbox.txt"
	check grep -q '^cs1 0 free' "$d/evidence/selftest/consumer.txt"
	check grep -q 'canary event 1' "$d/evidence/selftest/log.txt"
	# The refusals: a directory without the marker, an existing init target.
	check sh -c '! sh "$0" fire "$1" 2 2>/dev/null' "$0" "$base"
	check sh -c '! sh "$0" init "$1" 2>/dev/null' "$0" "$d"
	echo "selftest: ok ($d; nothing removed)"
}

[ $# -ge 1 ] || usage
cmd=$1
shift
case $cmd in
init) init "$@" ;;
fire) fire "$@" ;;
capture) capture "$@" ;;
selftest) selftest "$@" ;;
*) usage ;;
esac
