# Releasing

Releases are built on a maintainer's machine. There are no GitHub Actions:
nothing builds or publishes on push.

The released `taskr` is the Rust build (`crates/taskr`). The Go sources at the
repository root stay until the Rust test port is complete; until then they only
serve as a test oracle in the release gate (see the contract tests in
`tools/contract/`).

## release.sh

```sh
./release.sh v1.2.3 --dry-run              # build and check everything; publish nothing
./release.sh v1.2.3                        # build, tag, push the tag, create a prerelease
./release.sh v1.2.3 --promote              # after the 24 h watch: make it the latest release
./release.sh v1.2.3 --darwin-only          # only build the darwin pair on the Mac, into dist-darwin/
./release.sh v1.2.3 --darwin-from DIR      # release with a darwin pair built by hand
```

The tag must look like `vMAJOR.MINOR.PATCH`; a `dev` or other build version
is refused. The script runs on Linux and:

1. **Preflight.** Refuses a dirty worktree (a dry run skips this check). Without
   `--dry-run` it also refuses an existing tag and a `HEAD` that is not in
   `origin/master`. It checks the tools below and the Mac's ssh login; on a
   missing tool it prints the install block from `tools/contract/README.md`.
2. **Gate.** `cargo fmt --check`, then `cargo clippy -D warnings` and
   `cargo test --workspace`, each with default features and with
   `--features contract`. While Go exists, the Go adapter suite also runs
   against the contract build (`TASKR_BIN`) under `/tmp/taskr-test.lock`: all
   196 binary-eligible tests must pass, and the only other outcome allowed is
   `skipped-internals`. Tests run with a scratch HOME and ledger under
   `.scratch/release/`, without task identity or any inherited `TASKR_CONTRACT_*`
   filter.
3. **Linux half.** `tools/release-rust.sh` with `TASKR_VERSION=<tag>`: static
   musl binaries for x86_64 and aarch64 through `cargo zigbuild`, a refusal of
   any test-only hook, and a version smoke of the native binary.
4. **Darwin half**, on the Mac over ssh (there is no CI): `mktemp -d` there,
   `git archive HEAD` into it, then `tools/release-rust.sh` with the same
   version, which smokes the native arm64 binary. Both binaries are copied back,
   checked against the Mac's `SHA256SUMS`, and the Mac directory is removed.
   darwin/amd64 gets a static check only (`file` reports an x86_64 Mach-O),
   because the Mac is arm64 without Rosetta. `TASKR_RELEASE_SSH` replaces the
   whole remote command (default `ssh -o BatchMode=yes -o ConnectTimeout=10
   olafurns@olafurns-mbp`); tests set it to a local fake, so they never reach
   the Mac.
5. **Assets.** The binaries are renamed `taskr-{linux,darwin}-{amd64,arm64}`.
   Just before the checksums, each of the four must have the expected `file`
   type, contain the tag, and contain no test-only hook (`TASKR_FROZEN_NOW`,
   `TASKR_CONTRACT_`, `--contract-`), whether it was built here or imported.
   linux/amd64 must print the tag from `--json version` (linux/arm64 too when
   `qemu-aarch64` is installed).
6. **Packaging.** `SKILL.md`, `install.sh`, `skill.tar.gz` (the skill and
   `references/`) and `plugin.tar.gz` (the Herdr plugin, stamped with the
   version), plus one `SHA256SUMS` over all eight, in `dist/`.
7. **Publish**, without `--dry-run`: tag, push the tag, and
   `gh release create --prerelease`.

The nine asset names are the ones `install.sh` and the Herdr plugin fetch, so
neither changes between releases.

### Prerelease, watch, promote

A new release is a GitHub prerelease. `install.sh` without `TASKR_VERSION`
resolves `releases/latest`, which skips prereleases, so fresh installs keep the
previous release until the new one has run for 24 hours on the canary hosts
(an empty spool, no daemon errors, RPCs succeeding). Install it on a canary
with `TASKR_VERSION=<tag>`. After the watch:

```sh
./release.sh v1.2.3 --promote   # gh release edit v1.2.3 --prerelease=false --latest
```

`--promote` first reads the current latest release (`gh release view`) and
refuses a tag older than it, so `latest` never moves back. `TASKR_RELEASE_GH`
replaces the `gh` command; tests set it to a fake.

`sh tools/release-test.sh` runs the refusal tests (test-only hooks in an imported
darwin pair, the adapter summary, an older `--promote`) with both fakes.

### Without the Mac

If the Mac is unreachable, build the pair there by hand later, or on any
Apple-silicon Mac with the Rust toolchain:

```sh
TASKR_VERSION=v1.2.3 tools/release-rust.sh    # on the Mac, from a checkout of the release commit
```

Copy `dist-rust/taskr-rust-aarch64-apple-darwin`,
`dist-rust/taskr-rust-x86_64-apple-darwin` and `dist-rust/SHA256SUMS` into a
directory and pass it with `--darwin-from DIR`. It is checked (checksums,
Mach-O type, tag, no test-only hook) before the gate.
`./release.sh <tag> --darwin-only` does the same build over ssh into
`dist-darwin/`.

### Prerequisites

On the Linux host: `git`, Rust from `rust-toolchain.toml` (1.99.0 with the
musl and darwin targets), `zig` and `cargo-zigbuild` (user-space install in
`tools/contract/README.md`), `python3`, `file`, `strings`, `sha256sum`, `tar`,
`ssh`, and while Go exists, `go` and `flock`. Publishing needs an authenticated
`gh`. On the Mac: Rust from `rust-toolchain.toml` with both darwin targets, and
`python3` (the command-line tools).

### Rollback

The previous release stays the rollback; v0.16.1 is the last Go release and
shares the ledger, identity record and spool with the Rust build. On a host:

1. download `taskr-<os>-<arch>` from the v0.16.1 release (or
   `TASKR_VERSION=v0.16.1 sh install.sh`);
2. swap it in for the installed `taskr`;
3. run `taskr daemon --restart`.

## tools/release-rust.sh

```sh
TASKR_VERSION=v1.2.3 tools/release-rust.sh
```

It takes no arguments; the `TASKR_VERSION` default is
`git describe --always --dirty`. It builds `taskr` for the host's platform
family (Linux: static musl x86_64 and aarch64; macOS: arm64 and x86_64),
refuses a binary that contains a test-only hook, runs the native binary once
(`version`, `help`, `status`) in an empty scratch home, and writes
`dist-rust/taskr-rust-<target>` and `dist-rust/SHA256SUMS`. It creates no tag,
uploads nothing and does not touch the installed binary. `release.sh` runs it
for both halves.

## taskr-tui

`taskr-tui` is not part of the release yet. Build it with
`cargo install --path crates/taskr-tui`; a prebuilt release asset is planned.

Its screenshots under `docs/images/` come from the synthetic fixture:

```sh
cargo run -p taskr-tui --example frames -- frames   # text and ANSI frames
python3 crates/taskr-tui/tools/png.py frames        # PNGs (needs Pillow and DejaVu Sans Mono)
```

The four README images are the 120x40 glance and campaign frames, dark and
light, reduced to a 256-colour palette to keep them small:

```sh
python3 - <<'PY'
from PIL import Image
for frame, name in [("glance-120x40", "glance-dark"), ("glance-120x40-light", "glance-light"),
                    ("campaign-120x40", "campaign-dark"), ("campaign-120x40-light", "campaign-light")]:
    image = Image.open(f"frames/{frame}.png").quantize(colors=256, dither=Image.Dither.NONE)
    image.save(f"docs/images/{name}.png", optimize=True)
PY
```

`cargo test -p taskr-tui` compares every frame with `crates/taskr-tui/tests/golden/`.
