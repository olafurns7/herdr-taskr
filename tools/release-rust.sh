#!/bin/sh
# Build local Rust assets only. No tags, uploads, or installed binary changes.
set -eu
if [ "$#" -ne 0 ]; then
    echo 'usage: tools/release-rust.sh (TASKR_VERSION defaults to git describe --always --dirty)' >&2
    exit 2
fi
cd "$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$HOME/.cache/taskr-tools/cargo/bin:$HOME/.cache/taskr-tools/bin:$PATH"
: "${CARGO_TARGET_DIR:=$PWD/target}"
case "$CARGO_TARGET_DIR" in /*) ;; *) CARGO_TARGET_DIR="$PWD/$CARGO_TARGET_DIR" ;; esac
export CARGO_TARGET_DIR
: "${TASKR_VERSION:=$(git describe --always --dirty)}"
: "${TASKR_VERSION:?release-rust: TASKR_VERSION must not be empty}"
export TASKR_VERSION
case "$(uname -s)" in
    Linux) targets='x86_64-unknown-linux-musl aarch64-unknown-linux-musl' ;;
    Darwin) targets='aarch64-apple-darwin x86_64-apple-darwin' ;;
    *) echo 'release-rust: Linux or macOS required' >&2; exit 2 ;;
esac
mkdir -p dist-rust
stage=$(mktemp -d "$PWD/dist-rust/.build.XXXXXX")
trap 'case "$stage" in "$PWD"/dist-rust/.build.??????) rm -rf -- "$stage" ;; *) echo "release-rust: refusing cleanup of $stage" >&2 ;; esac' 0
for target in $targets; do
    case "$target" in
        # Let the target linker strip symbols; host strip cannot strip ARM ELF.
        *linux*) cargo zigbuild --locked --release --config 'profile.release.strip="symbols"' -p taskr --target "$target" ;;
        *darwin*) cargo build --locked --release --config 'profile.release.strip="symbols"' -p taskr --target "$target" ;;
    esac
    asset="taskr-rust-$target"
    cp "$CARGO_TARGET_DIR/$target/release/taskr" "$stage/$asset"
    strings -a "$stage/$asset" > "$stage/strings"
    if grep -E 'TASKR_FROZEN_NOW|TASKR_CONTRACT_|--contract-' "$stage/strings"; then
        echo "release-rust: contract hook found in $asset" >&2
        exit 1
    fi
done
case "$(uname -m):$(uname -s)" in
    x86_64:Linux) native=x86_64-unknown-linux-musl ;;
    aarch64:Linux|arm64:Linux) native=aarch64-unknown-linux-musl ;;
    arm64:Darwin) native=aarch64-apple-darwin ;;
    x86_64:Darwin) native=x86_64-apple-darwin ;;
    *) echo 'release-rust: no native smoke target' >&2; exit 2 ;;
esac
mkdir "$stage/home" "$stage/bin"
for tool in herdr tailscale; do
    printf '#!/bin/sh\nexit 2\n' > "$stage/bin/$tool"
    chmod +x "$stage/bin/$tool"
done
version=$(env -i HOME="$stage/home" TASKR_DB="$stage/home/smoke.db" \
    PATH="$stage/bin:/usr/bin:/bin" HERDR_SOCKET_PATH="$stage/home/absent.sock" \
    "$stage/taskr-rust-$native" --json version)
printf '%s\n' "$version" | python3 -c 'import json, os, sys; assert json.load(sys.stdin)["version"] == os.environ["TASKR_VERSION"]' || {
    echo "release-rust: unexpected version: $version (wanted $TASKR_VERSION)" >&2
    exit 1
}
printf '%s\n' "$version"
for cmd in help status; do
    env -i HOME="$stage/home" TASKR_DB="$stage/home/smoke.db" \
        PATH="$stage/bin:/usr/bin:/bin" HERDR_SOCKET_PATH="$stage/home/absent.sock" \
        "$stage/taskr-rust-$native" "$cmd"
done
for target in $targets; do
    mv -f "$stage/taskr-rust-$target" "dist-rust/taskr-rust-$target"
done
(
    cd dist-rust
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum taskr-rust-* > SHA256SUMS
    else
        shasum -a 256 taskr-rust-* > SHA256SUMS
    fi
    cat SHA256SUMS
)
