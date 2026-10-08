#!/bin/sh
set -eu

usage() {
	printf '%s\n' 'usage: ./release.sh <tag> [--dry-run]' >&2
	exit 2
}

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || usage
tag=$1
dry_run=0
if [ "$#" -eq 2 ]; then
	[ "$2" = --dry-run ] || usage
	dry_run=1
fi
printf '%s\n' "$tag" | grep -E -q '^v[0-9]+\.[0-9]+\.[0-9]+$' || usage

if [ "$dry_run" -eq 0 ]; then
	dirty=$(git status --porcelain) || {
		printf '%s\n' 'release.sh: could not inspect git status' >&2
		exit 1
	}
	[ -z "$dirty" ] || {
		printf '%s\n' 'release.sh: refusing to release with a dirty worktree' >&2
		exit 1
	}
fi

TASKR_DB="$(mktemp -d)/guard.db" go vet ./...
TASKR_DB="$(mktemp -d)/guard.db" go test ./... -count=1 -timeout 20m

rm -rf dist && mkdir dist
for target in darwin/arm64 darwin/amd64 linux/amd64 linux/arm64; do
	os=${target%/*}
	arch=${target#*/}
	CGO_ENABLED=0 GOOS="$os" GOARCH="$arch" go build -trimpath -ldflags "-s -w -X main.version=$tag" -o "dist/taskr-$os-$arch" .
done
cp SKILL.md install.sh dist/
COPYFILE_DISABLE=1 tar -czf dist/skill.tar.gz SKILL.md references
# plugin.tar.gz holds plugin/ flat, with the manifest version set from the tag.
plugin_tmp=$(mktemp -d)
trap 'rm -rf "$plugin_tmp"' 0
cp plugin/run.sh "$plugin_tmp/run.sh"
sed "s/^version = \".*\"$/version = \"${tag#v}\"/" plugin/herdr-plugin.toml > "$plugin_tmp/herdr-plugin.toml"
grep -q "^version = \"${tag#v}\"$" "$plugin_tmp/herdr-plugin.toml" || {
	printf '%s\n' 'release.sh: could not set the plugin version' >&2
	exit 1
}
COPYFILE_DISABLE=1 tar -czf dist/plugin.tar.gz -C "$plugin_tmp" run.sh herdr-plugin.toml
(
	cd dist
	shasum -a 256 taskr-darwin-arm64 taskr-darwin-amd64 taskr-linux-amd64 taskr-linux-arm64 SKILL.md install.sh skill.tar.gz plugin.tar.gz > SHA256SUMS
	cat SHA256SUMS
)

if [ "$dry_run" -eq 1 ]; then
	exit 0
fi

git fetch -q origin master
git merge-base --is-ancestor HEAD origin/master || { printf '%s\n' 'release.sh: HEAD is not in origin/master; push it to master first' >&2; exit 1; }

git tag -a "$tag" -m "$tag"
git push origin "$tag"
gh release create "$tag" dist/* --repo olafurns7/herdr-taskr --title "$tag" --notes "Prebuilt taskr binaries for macOS and Linux (arm64 and amd64), checksums, install script, skill, and plugin. See https://github.com/olafurns7/herdr-taskr/blob/master/docs/install.md for agent-guided installation."
