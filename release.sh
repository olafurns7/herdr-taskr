#!/bin/sh
# Build the Rust taskr release (Linux here, darwin on the Mac over ssh) and
# publish it as a GitHub prerelease. See docs/release.md.
set -eu

repo=olafurns7/herdr-taskr
# The darwin half's remote command; tests point it at a fake, never at the Mac.
remote=${TASKR_RELEASE_SSH:-ssh -o BatchMode=yes -o ConnectTimeout=10 olafurns@olafurns-mbp}
# The GitHub CLI; tests point it at a fake, never at the real repository.
gh=${TASKR_RELEASE_GH:-gh}
work=.scratch/release

usage() {
	printf '%s\n' 'usage: ./release.sh <tag> [--dry-run] [--darwin-from DIR]' \
		'       ./release.sh <tag> --darwin-only [--dry-run]' \
		'       ./release.sh <tag> --promote' >&2
	exit 2
}

die() {
	printf 'release.sh: %s\n' "$*" >&2
	exit 1
}

# Run one command on the Mac through $remote (word-split on purpose).
on_mac() {
	# shellcheck disable=SC2086
	$remote "$@"
}

[ "$#" -ge 1 ] || usage
tag=$1
shift
mode=release
dry_run=0
darwin_from=
while [ "$#" -gt 0 ]; do
	case $1 in
	--dry-run) dry_run=1 ;;
	--darwin-from) [ "$#" -ge 2 ] || usage; darwin_from=$2; shift ;;
	--darwin-only) [ "$mode" = release ] || usage; mode=darwin ;;
	--promote) [ "$mode" = release ] || usage; mode=promote ;;
	*) usage ;;
	esac
	shift
done
[ "$mode" != promote ] || { [ "$dry_run" -eq 0 ] && [ -z "$darwin_from" ]; } || usage
[ "$mode" != darwin ] || [ -z "$darwin_from" ] || usage
# Only vMAJOR.MINOR.PATCH: this refuses 'dev' and every other build version.
printf '%s\n' "$tag" | grep -E -q '^v[0-9]+\.[0-9]+\.[0-9]+$' || usage

cd "$(dirname -- "$0")"
export PATH="$HOME/.cargo/bin:$HOME/.cache/taskr-tools/cargo/bin:$HOME/.cache/taskr-tools/bin:$PATH"

if [ "$mode" = promote ]; then
	# After the 24 h watch: make the prerelease the release install.sh takes by
	# default, but never move "latest" back to an older tag.
	latest=$("$gh" release view --repo "$repo" --json tagName --jq .tagName) || die 'could not read the latest release'
	printf '%s\n' "$latest" | grep -E -q '^v[0-9]+\.[0-9]+\.[0-9]+$' || die "unexpected latest release tag: $latest"
	newest=$(printf '%s\n%s\n' "${latest#v}" "${tag#v}" | sort -t. -k1,1n -k2,2n -k3,3n | tail -n 1)
	[ "$newest" = "${tag#v}" ] || die "refusing to promote $tag: it is older than the latest release $latest"
	"$gh" release edit "$tag" --repo "$repo" --prerelease=false --latest
	exit 0
fi

# --- Preflight ---------------------------------------------------------------

tools='git file strings sha256sum'
[ -n "$darwin_from" ] || tools="$tools ${remote%% *}"
if [ "$mode" = release ]; then
	[ "$(uname -s)" = Linux ] || die 'the release builds on Linux; the darwin half runs on the Mac over ssh'
	tools="$tools cargo rustup zig cargo-zigbuild python3 tar sed awk"
	[ ! -f go.mod ] || tools="$tools go flock"
	[ "$dry_run" -eq 1 ] || tools="$tools $gh"
fi
missing=
for tool in $tools; do
	command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ "$mode" = release ] && command -v rustup >/dev/null 2>&1; then
	installed=$(rustup target list --installed)
	for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
		printf '%s\n' "$installed" | grep -qx "$target" || missing="$missing rustup-target:$target"
	done
fi
if [ -n "$missing" ]; then
	{
		printf 'release.sh: missing:%s\n\n' "$missing"
		printf '%s\n\n' 'Install the build tools in user space (tools/contract/README.md):'
		awk '/^## Static musl builds/ { s = 1 } s && /^```/ { if (p) exit; p = 1; next } p' tools/contract/README.md
		printf '\n%s\n%s\n' 'rust-toolchain.toml pins 1.99.0, so add the targets to that toolchain:' \
			'rustup target add --toolchain 1.99.0 x86_64-unknown-linux-musl aarch64-unknown-linux-musl'
	} >&2
	exit 1
fi

if [ "$dry_run" -eq 0 ]; then
	dirty=$(git status --porcelain) || die 'could not inspect git status'
	[ -z "$dirty" ] || die 'refusing to release with a dirty worktree'
fi
if [ "$mode" = release ] && [ "$dry_run" -eq 0 ]; then
	! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || die "tag $tag already exists"
	git fetch -q origin master
	git merge-base --is-ancestor HEAD origin/master || die 'HEAD is not in origin/master; push it to master first'
fi
if [ -z "$darwin_from" ]; then
	on_mac true || die "cannot reach the Mac ($remote); build darwin by hand and pass --darwin-from DIR"
fi

rm -rf .scratch/release
mkdir -p .scratch/release/home

# file(1) wording, the tag compiled into the binary, and no test-only hook
# (the same strings tools/release-rust.sh refuses).
static_check() {
	file -b "$1" | grep -E -q "$2" || die "$1 is not $2: $(file -b "$1")"
	strings -a "$1" | grep -F -q "$tag" || die "$1 does not contain $tag"
	! strings -a "$1" | grep -E -q 'TASKR_FROZEN_NOW|TASKR_CONTRACT_|--contract-' || die "$1 contains a test-only hook"
}

# The JSON version of a binary that runs here, in an empty scratch home.
run_version() {
	out=$(env -i HOME="$work/home" TASKR_DB="$work/home/version.db" PATH=/usr/bin:/bin "$@" --json version) ||
		die "$* --json version failed"
	got=$(printf '%s\n' "$out" | python3 -c 'import json, sys; print(json.load(sys.stdin)["version"])')
	[ "$got" = "$tag" ] || die "$* reports version $got, wanted $tag"
	printf '%s: %s\n' "$*" "$out"
}

# --- Darwin half: the Mac builds HEAD; we keep only checksummed copies ---------

darwin_build() {
	rm -rf dist-darwin
	mkdir dist-darwin
	macdir=$(on_mac 'mktemp -d /tmp/taskr-release.XXXXXX')
	printf '%s\n' "$macdir" | grep -E -qx '/tmp/taskr-release\.[A-Za-z0-9]{6}' || die "unexpected Mac scratch dir: $macdir"
	trap 'on_mac "rm -rf -- $macdir"' 0
	git archive --format=tar HEAD | on_mac "tar -xf - -C $macdir"
	# release-rust.sh smokes the native arm64 binary there (version, help, status).
	on_mac "cd $macdir && TASKR_VERSION=$tag sh tools/release-rust.sh"
	for name in SHA256SUMS taskr-rust-aarch64-apple-darwin taskr-rust-x86_64-apple-darwin; do
		on_mac "cat $macdir/dist-rust/$name" > "dist-darwin/$name"
	done
	on_mac "rm -rf -- $macdir"
	trap - 0
}

# Both darwin files must match the Mac's SHA256SUMS and be the right Mach-O.
darwin_check() {
	for pair in aarch64:arm64 x86_64:x86_64; do
		name=taskr-rust-${pair%:*}-apple-darwin
		want=$(awk -v n="$name" '$2 == n || $2 == "*" n { print $1 }' "$1/SHA256SUMS")
		got=$(sha256sum < "$1/$name")
		[ -n "$want" ] && [ "$want" = "${got%% *}" ] || die "$1/$name does not match $1/SHA256SUMS"
		static_check "$1/$name" "^Mach-O 64-bit .*${pair#*:}"
	done
}

# A hand-built pair is checked now, before the long gate.
[ -z "$darwin_from" ] || darwin_check "$darwin_from"

if [ "$mode" = darwin ]; then
	darwin_build
	darwin_check dist-darwin
	cat dist-darwin/SHA256SUMS
	printf '%s\n' "release.sh: dist-darwin is ready: ./release.sh $tag --darwin-from dist-darwin"
	exit 0
fi

# --- Gate (decision 1A, the interim gate) --------------------------------------

cargo fmt --all --check
(
	# Tests get a scratch HOME and ledger and no task identity; the build caches stay put.
	export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
	if [ -f go.mod ]; then
		GOCACHE=$(go env GOCACHE) GOMODCACHE=$(go env GOMODCACHE) GOPATH=$(go env GOPATH) GOENV=$(go env GOENV)
		export GOCACHE GOMODCACHE GOPATH GOENV
	fi
	export HOME="$PWD/$work/home" TASKR_DB="$PWD/$work/home/gate.db"
	unset TASKR_TASK TASKR_LAUNCH TASKR_BIN TASKR_FROZEN_NOW
	# An inherited filter such as TASKR_CONTRACT_FAMILY would shrink the adapter suite.
	for var in $(env | sed -n 's/^\(TASKR_CONTRACT_[A-Za-z0-9_]*\)=.*/\1/p'); do
		unset "$var"
	done
	for features in '' '--features contract'; do
		# shellcheck disable=SC2086 # $features is empty or two words
		cargo clippy --locked --workspace --all-targets $features -- -D warnings
		# shellcheck disable=SC2086
		cargo test --locked --workspace $features
	done
	[ -f go.mod ] || exit 0
	# While Go exists: its adapter suite against the contract build. All 196
	# binary-eligible tests must pass; the only other outcome allowed is
	# skipped-internals (Go white-box and in-process tests).
	cargo build --locked --release -p taskr --features contract
	flock /tmp/taskr-test.lock env TASKR_BIN="$PWD/target/release/taskr" \
		TASKR_CONTRACT_SUMMARY="$PWD/$work/adapter-summary.json" go test -timeout 20m -count=1 ./...
	python3 - "$work/adapter-summary.json" <<'EOF'
import json, sys
total = {}
for counts in json.load(open(sys.argv[1])).values():
    for outcome, n in counts.items():
        total[outcome] = total.get(outcome, 0) + n
print("adapter " + " ".join(f"{k}={v}" for k, v in sorted(total.items())))
other = {k: v for k, v in total.items() if k not in ("pass", "skipped-internals") and v}
if total.get("pass") != 196 or other:
    sys.exit("release.sh: the adapter suite needs pass=196 and no mismatch, not-implemented or other skip")
EOF
)

# --- Build ---------------------------------------------------------------------

# Linux half: static musl, the contract-hook check and the native version smoke.
TASKR_VERSION=$tag sh tools/release-rust.sh
if [ -z "$darwin_from" ]; then
	darwin_build
	darwin_from=dist-darwin
	darwin_check dist-darwin
fi

rm -rf dist && mkdir dist
cp dist-rust/taskr-rust-x86_64-unknown-linux-musl dist/taskr-linux-amd64
cp dist-rust/taskr-rust-aarch64-unknown-linux-musl dist/taskr-linux-arm64
cp "$darwin_from/taskr-rust-aarch64-apple-darwin" dist/taskr-darwin-arm64
cp "$darwin_from/taskr-rust-x86_64-apple-darwin" dist/taskr-darwin-amd64
chmod 755 dist/taskr-*

run_version dist/taskr-linux-amd64
if command -v qemu-aarch64 >/dev/null 2>&1; then
	run_version qemu-aarch64 dist/taskr-linux-arm64
else
	printf '%s\n' 'release.sh: no qemu-aarch64: linux/arm64 has the static check only'
fi
# darwin/arm64 ran on the Mac (release-rust.sh); darwin/amd64 has the static check only (decision 2A).

cp SKILL.md install.sh dist/
tar -czf dist/skill.tar.gz SKILL.md references
# plugin.tar.gz holds plugin/ flat, with the manifest version set from the tag.
mkdir "$work/plugin"
cp plugin/run.sh "$work/plugin/run.sh"
sed "s/^version = \".*\"$/version = \"${tag#v}\"/" plugin/herdr-plugin.toml > "$work/plugin/herdr-plugin.toml"
grep -q "^version = \"${tag#v}\"$" "$work/plugin/herdr-plugin.toml" || die 'could not set the plugin version'
tar -czf dist/plugin.tar.gz -C "$work/plugin" run.sh herdr-plugin.toml
# Every shipped binary, imported or built here, gets the same final check.
static_check dist/taskr-linux-amd64 '^ELF 64-bit LSB .*x86-64.*statically linked'
static_check dist/taskr-linux-arm64 '^ELF 64-bit LSB .*ARM aarch64.*statically linked'
static_check dist/taskr-darwin-arm64 '^Mach-O 64-bit .*arm64'
static_check dist/taskr-darwin-amd64 '^Mach-O 64-bit .*x86_64'
(
	cd dist
	sha256sum taskr-darwin-arm64 taskr-darwin-amd64 taskr-linux-amd64 taskr-linux-arm64 SKILL.md install.sh skill.tar.gz plugin.tar.gz > SHA256SUMS
	cat SHA256SUMS
)

if [ "$dry_run" -eq 1 ]; then
	printf '%s\n' "release.sh: dry run: dist/ is ready; nothing tagged, pushed or uploaded"
	exit 0
fi

# --- Publish: a prerelease until the 24 h watch, then --promote ------------------

git tag -a "$tag" -m "$tag"
git push origin "$tag"
"$gh" release create "$tag" dist/* --repo "$repo" --title "$tag" --prerelease --notes "Prebuilt taskr binaries for macOS and Linux (arm64 and amd64), checksums, install script, skill, and plugin. See https://github.com/olafurns7/herdr-taskr/blob/master/docs/install.md for agent-guided installation."
printf '%s\n' "release.sh: $tag is a prerelease; after the 24 h watch run ./release.sh $tag --promote"
