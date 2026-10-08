# Releasing

Releases are built on a maintainer's machine. There are no GitHub Actions:
nothing builds or publishes on push.

taskr currently has two implementations of the same CLI and daemon, held to
the same behaviour by the contract tests in `tools/contract/`:

| | Go (repository root) | Rust (`crates/taskr`) |
| --- | --- | --- |
| Script | `./release.sh` | `tools/release-rust.sh` |
| Output | `dist/` | `dist-rust/` |
| Publishes | Yes: tag and GitHub release | No: local files only |

`install.sh` installs the Go binary from the GitHub release.

## Go: release.sh

```sh
./release.sh v1.2.3 --dry-run   # build and check everything; publish nothing
./release.sh v1.2.3             # build, tag, push the tag, create the release
```

The tag must look like `vMAJOR.MINOR.PATCH`. The script:

1. refuses a dirty worktree (a dry run skips this check);
2. runs `go vet ./...` and `go test ./...`;
3. builds `taskr` for macOS and Linux, arm64 and amd64, with the tag as the
   version;
4. packs `install.sh`, `SKILL.md`, `skill.tar.gz` (the skill and
   `references/`) and `plugin.tar.gz` (the Herdr plugin, stamped with the
   version) into `dist/`, with `SHA256SUMS`;
5. without `--dry-run`: checks that `HEAD` is already in `origin/master`, then
   tags, pushes the tag and runs `gh release create`.

It needs Go, `git`, `tar`, `shasum` and, to publish, an authenticated `gh`.

## Rust: tools/release-rust.sh

```sh
tools/release-rust.sh
```

It takes no arguments. `TASKR_VERSION` sets the version; the default is
`git describe --always --dirty`. The script:

1. builds `taskr` for the host's platform family: on Linux, static musl
   binaries for x86_64 and aarch64 (through `cargo zigbuild`); on macOS, arm64
   and x86_64;
2. refuses a binary that contains a test-only hook;
3. runs the native binary once (`version`, `help`, `status`) in an empty
   scratch home;
4. writes `dist-rust/taskr-rust-<target>` and `dist-rust/SHA256SUMS`.

It creates no tag, uploads nothing and does not touch the installed binary.

## taskr-tui

`taskr-tui` is not part of either script yet. Build it with
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
