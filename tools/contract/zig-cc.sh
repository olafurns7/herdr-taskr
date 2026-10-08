#!/usr/bin/env bash
# cc-rs adds the Rust/clang triple; Zig names this target without "unknown".
args=()
for arg; do
    if [[ "$arg" != --target=x86_64-unknown-linux-musl ]]; then
        args+=("$arg")
    fi
done
exec zig cc -target x86_64-linux-musl "${args[@]}"
