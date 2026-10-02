#!/bin/sh
set -eu

REPO=olafurns7/herdr-taskr

fail() {
	printf 'install.sh: %s\n' "$*" >&2
	exit 1
}

kernel=$(uname -s)
case $kernel in
	Darwin) OS=darwin ;;
	Linux) OS=linux ;;
	*) fail "unsupported operating system: $kernel" ;;
esac

machine=$(uname -m)
case $machine in
	arm64|aarch64) ARCH=arm64 ;;
	x86_64|amd64) ARCH=amd64 ;;
	*) fail "unsupported architecture: $machine" ;;
esac

ASSET="taskr-$OS-$ARCH"
TAG=${TASKR_VERSION:-}
taskr_home=${HOME:-}
[ -n "$taskr_home" ] || fail 'HOME is not set'
DIR=${TASKR_INSTALL_DIR:-$taskr_home/.local/bin}
SKILL_DIR=${TASKR_SKILL_DIR:-$taskr_home/.agents/skills/taskr}
PLUGIN_DIR=$taskr_home/.local/share/taskr/plugin
PLUGIN_ID=olafurns7.taskr
with_skill=1
[ "${TASKR_NO_SKILL:-}" != 1 ] || with_skill=0
with_plugin=1
[ "${TASKR_NO_PLUGIN:-}" != 1 ] || with_plugin=0
token=${GH_TOKEN:-${GITHUB_TOKEN:-}}

umask 077
tmp=$(mktemp -d) || fail 'could not create temporary directory'
trap 'rm -rf "$tmp"' 0
trap 'exit 1' HUP INT TERM

download_with_gh() {
	set -- --repo "$REPO" --pattern "$ASSET" --pattern SHA256SUMS --dir "$tmp" --clobber
	[ "$with_skill" -eq 0 ] || set -- "$@" --pattern skill.tar.gz
	[ "$with_plugin" -eq 0 ] || set -- "$@" --pattern plugin.tar.gz
	if [ -n "$TAG" ]; then
		gh release download "$TAG" "$@"
	else
		gh release download "$@"
	fi
}

curl_api() {
	accept_type=$1
	url=$2
	output=$3
	printf 'header = "Authorization: Bearer %s"\nheader = "Accept: %s"\n' "$token" "$accept_type" > "$tmp/curl.conf"
	curl -q -f -L -sS --config "$tmp/curl.conf" --output "$output" "$url"
}

# asset_id prints the id of the named asset in $tmp/release.json. GitHub may
# send compact or pretty JSON, so the layout is normalized first: newlines are
# dropped, the "assets" array is cut out, URL templates such as {/other_user}
# are dropped so that each asset's nested "uploader" object (which has its own
# "id") can be removed whole, and "}" ends each asset's line.
asset_id() {
	name=$1
	pairs=$(tr -d '\n\r' < "$tmp/release.json" | sed -n '
		/"assets"[[:space:]]*:[[:space:]]*\[/ {
			s/.*"assets"[[:space:]]*:[[:space:]]*\[//
			s/{\/[^}]*}//g
			s/"uploader"[[:space:]]*:[[:space:]]*{[^}]*}//g
			s/\].*//
			p
		}
	' | tr '}' '\n' | sed -n '
		s/.*"id"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*"name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1 \2/p
		t
		s/.*"name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*"id"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\2 \1/p
	')
	id=$(printf '%s\n' "$pairs" | while read -r pair_id pair_name; do
		if [ "$pair_name" = "$name" ]; then
			printf '%s\n' "$pair_id"
			break
		fi
	done)
	[ -n "$id" ] || fail "release asset not found: $name"
	printf '%s\n' "$id"
}

if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
	if ! download_with_gh; then
		fail 'could not download release assets with gh'
	fi
elif [ -n "$token" ]; then
	command -v curl >/dev/null 2>&1 || fail 'curl is required when using GH_TOKEN or GITHUB_TOKEN'
	if [ -n "$TAG" ]; then
		api_url="https://api.github.com/repos/$REPO/releases/tags/$TAG"
	else
		api_url="https://api.github.com/repos/$REPO/releases/latest"
	fi
	if ! curl_api application/vnd.github+json "$api_url" "$tmp/release.json"; then
		fail 'could not download release metadata'
	fi
	names="$ASSET SHA256SUMS"
	[ "$with_skill" -eq 0 ] || names="$names skill.tar.gz"
	[ "$with_plugin" -eq 0 ] || names="$names plugin.tar.gz"
	for name in $names; do
		id=$(asset_id "$name")
		if ! curl_api application/octet-stream "https://api.github.com/repos/$REPO/releases/assets/$id" "$tmp/$name"; then
			fail "could not download release asset: $name"
		fi
	done
else
	command -v curl >/dev/null 2>&1 || fail 'curl or gh is required'
	names="$ASSET SHA256SUMS"
	[ "$with_skill" -eq 0 ] || names="$names skill.tar.gz"
	[ "$with_plugin" -eq 0 ] || names="$names plugin.tar.gz"
	for name in $names; do
		if [ -n "$TAG" ]; then
			url="https://github.com/$REPO/releases/download/$TAG/$name"
		else
			url="https://github.com/$REPO/releases/latest/download/$name"
		fi
		if ! curl -q -f -L -sS --output "$tmp/$name" "$url"; then
			fail "could not download release asset: $name"
		fi
	done
fi

verify() {
	grep -F "  $1" "$tmp/SHA256SUMS" > "$tmp/asset.checksum" || fail "checksum entry not found: $1"
	if command -v shasum >/dev/null 2>&1; then
		if ! (cd "$tmp" && shasum -a 256 -c "$tmp/asset.checksum"); then
			fail "checksum verification failed: $1"
		fi
	elif command -v sha256sum >/dev/null 2>&1; then
		if ! (cd "$tmp" && sha256sum -c "$tmp/asset.checksum"); then
			fail "checksum verification failed: $1"
		fi
	else
		fail 'shasum or sha256sum is required to verify downloads'
	fi
}
verify "$ASSET"
if [ "$with_skill" -eq 1 ]; then
	verify skill.tar.gz
	mkdir "$tmp/skill"
	tar -xzf "$tmp/skill.tar.gz" -C "$tmp/skill" || fail 'could not extract skill.tar.gz'
	[ -f "$tmp/skill/SKILL.md" ] && [ -d "$tmp/skill/references" ] || fail 'skill.tar.gz is missing core or references'
fi
[ "$with_plugin" -eq 0 ] || verify plugin.tar.gz

mkdir -p "$DIR"
chmod +x "$tmp/$ASSET"
mv -f "$tmp/$ASSET" "$DIR/taskr"
# The skill is readable like its neighbours (755/644), whatever the umask
# above. Agent links are opt-in and only touch existing config directories.
if [ "${TASKR_NO_SKILL:-}" != 1 ]; then
	(umask 022 && mkdir -p "$SKILL_DIR")
	chmod 755 "$SKILL_DIR"
	cp "$tmp/skill/SKILL.md" "$SKILL_DIR/SKILL.md"
	cp -R "$tmp/skill/references" "$SKILL_DIR/"
	find "$SKILL_DIR/references" -type d -exec chmod 755 {} +
	find "$SKILL_DIR/references" -type f -exec chmod 644 {} +
	chmod 644 "$SKILL_DIR/SKILL.md"
fi

version_output=$("$DIR/taskr" version)
printf 'installed %s/taskr (%s)\n' "$DIR" "$version_output"
if [ "${TASKR_NO_SKILL:-}" != 1 ]; then
	printf 'skill: %s/SKILL.md\n' "$SKILL_DIR"
	skill_path=$SKILL_DIR
	case $skill_path in
		/*) ;;
		*) skill_path=$PWD/$skill_path ;;
	esac
	skill_target=$(CDPATH= cd -P "$skill_path" 2>/dev/null && pwd -P) || fail "could not resolve skill directory: $SKILL_DIR"
	if [ "${TASKR_LINK_SKILLS:-}" = 1 ]; then
		found_agent=0
		for agent_dir in "$taskr_home/.claude" "$taskr_home/.codex" "$taskr_home/.config/opencode"; do
			[ -d "$agent_dir" ] || continue
			found_agent=1
			skills_dir=$agent_dir/skills
			if [ -d "$skills_dir" ]; then
				:
			elif [ -e "$skills_dir" ] || [ -L "$skills_dir" ]; then
				printf 'skill: conflict: %s exists but is not a directory; leaving it unchanged\n' "$skills_dir"
				continue
			elif ! mkdir -p "$skills_dir"; then
				printf 'skill: conflict: could not create %s; continuing\n' "$skills_dir"
				continue
			fi
			link=$skills_dir/taskr
			if [ -L "$link" ]; then
				linked_target=$(CDPATH= cd -P "$link" 2>/dev/null && pwd -P || :)
				if [ "$linked_target" = "$skill_target" ]; then
					printf 'skill: already linked: %s\n' "$link"
				else
					printf 'skill: conflict: %s is a different or dangling symlink; leaving it unchanged\n' "$link"
				fi
			elif [ -d "$link" ]; then
				printf 'skill: conflict: %s is an existing directory; leaving it unchanged\n' "$link"
			elif [ -e "$link" ]; then
				printf 'skill: conflict: %s is an existing file; leaving it unchanged\n' "$link"
			elif ln -s "$skill_target" "$link"; then
				printf 'skill: linked: %s\n' "$link"
			else
				printf 'skill: conflict: could not create %s; continuing\n' "$link"
			fi
		done
		[ "$found_agent" -eq 1 ] || printf '%s\n' 'skill: no existing agent config directories; no links created'
	else
		printf '%s\n' 'skill: optional links: set TASKR_LINK_SKILLS=1 to link into existing agent configs'
		printf 'skill: manual link: test -d "$HOME/.claude" && mkdir -p "$HOME/.claude/skills" && ln -s '
		printf "'%s'" "$(printf '%s' "$skill_target" | sed "s/'/'\\\\''/g")"
		printf ' "$HOME/.claude/skills/taskr"\n'
	fi
fi
# The Herdr plugin: files under $PLUGIN_DIR, linked into a running Herdr when
# this runs inside one. install.sh never starts or restarts the Herdr server.
if [ "$with_plugin" -eq 1 ]; then
	mkdir -p "$PLUGIN_DIR"
	tar -xzf "$tmp/plugin.tar.gz" -C "$PLUGIN_DIR" || fail 'could not extract plugin.tar.gz'
	printf 'plugin: %s\n' "$PLUGIN_DIR"
	manual="MANUAL: inside Herdr, run: herdr plugin link \"$PLUGIN_DIR\""
	if [ "${HERDR_ENV:-}" = 1 ] && command -v herdr >/dev/null 2>&1; then
		if ! plugins=$(herdr plugin list --json 2>/dev/null); then
			printf '%s\n' "$manual"
		else
			case $plugins in
				*"\"$PLUGIN_ID\""*) printf 'plugin: %s already linked\n' "$PLUGIN_ID" ;;
				*)
					if herdr plugin link "$PLUGIN_DIR"; then
						printf 'plugin: linked %s; the daemon starts at the next agent detection or Herdr start\n' "$PLUGIN_ID"
					else
						printf '%s\n' "$manual"
					fi
					;;
			esac
		fi
	else
		printf '%s\n' "$manual"
	fi
fi
case ":${PATH:-}:" in
	*":$DIR:"*) ;;
	*) printf 'install.sh: add %s to PATH\n' "$DIR" >&2 ;;
esac
