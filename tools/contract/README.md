# taskr contract harness

The harness runs explicit Rust binaries against synthetic fixtures and stored
golden observations. It never uses an installed taskr or live services. Child
commands receive scratch HOME/DB and fake Herdr/tailscale services.

## Build and check

```sh
cargo build --locked -p taskr --features contract
mkdir -p .scratch
cp target/debug/taskr .scratch/taskr-rust-contract
python3 tools/contract/selfcheck.py
python3 tools/contract/mapping_check.py
python3 tools/contract/cells.py --expect testdata/contract/golden \
  --rust .scratch/taskr-rust-contract
python3 tools/contract/run.py --expect testdata/contract/golden \
  --rust .scratch/taskr-rust-contract --catchup-fixtures
cargo test --locked --workspace
cargo test --locked --workspace --features contract
```

`golden_contract` runs the complete corpus as a Cargo integration test with
`--features contract`. Default tests run the seam-free child-context cell and
ignore `golden_contract`. Python 3 is required. On non-Linux systems the three
`/proc` process proof cells explicitly skip; portable cells still run.

`run.py` compares exit codes, stdout/stderr bytes and a digest of logical SQLite
state, including schema, sorted visible rows and FTS contents. Shadow tables and
empty extension tables absent from historical fixtures are excluded; integrity
and foreign keys are checked. Timeouts, signals and exit 125 fail the run.
`--family` selects an exact family or read/write/net/schema prefix and may repeat.
The full corpus uses `--catchup-fixtures` and must consume every observation.
RPC-child vectors use Rust's actual `--hub-child` context and synthetic stdin.
There is no live-ledger mode.

## Golden maintenance

The stored fixtures were originally recorded from the retired Go implementation;
producer labels and provenance remain historical evidence. Legacy spool and
process-identity records are replayed with current scratch paths, timestamps and
OS identities so Rust's compatibility checks still exercise the old formats.
The legacy response proxy uses the stored field set, with current fixture values.
All cells now run Rust on both sides of their reference/candidate fixtures.

Regenerate intentional changes from the Rust contract binary and review the diff:

```sh
python3 tools/contract/cells.py --record .scratch/taskr-rust-contract \
  --rust .scratch/taskr-rust-contract
```

`--record BIN` must identify the same file as `--rust BIN`. Unchanged observations
retain their original provenance; changed observations are marked `Rust rerecord`.
The Go oracle and its classifier were removed; neither can be regenerated here.

`golden.py` normalizes scratch prefixes, explicit binary paths, bound ephemeral
ports, process IDs/UIDs/start identities, real-time timestamps and heartbeat ages,
and local host metadata. It preserves suffixes, unbound ports, the production
`:7788` dashboard port, sentinel zero IDs, unrelated numbers and seeded frozen
timestamps. Runtime timing, cancellation, transport and OS identity assertions
remain active before normalization. `selfcheck.py` checks these boundaries, row
and FTS changes, changed exits, provenance retention and missing observations.

`run.json.gz` uses deterministic gzip (`mtime=0`), one observation per line.
Inspect with `gzip -cd testdata/contract/golden/run.json.gz`; `.gitattributes`
selects the gzip diff driver. Enable readable local diffs with
`git config diff.gzip.textconv "gzip -cd"`.

The `contract` feature enables RPC/tailnet/clock fixture seams and is off by
default. Production builds ignore `TASKR_FROZEN_NOW` and `TASKR_CONTRACT_*`;
never ship a contract build. Process/network cells use real time where needed.

## Historical coverage mapping

`testdata/contract/tests.tsv` is frozen classification data from the deleted Go
tests. Families, readiness, groups a/b/c and dependency reasons describe those
historical tests. The source classifier and adapter test-set checker were deleted.
No remaining gate calls them; `mapping_check.py` never validated tests.tsv, so no
replacement classification check is needed.

`mapping.tsv` retains historical Go test/subtest/file columns and Rust-only rows
(`go_test` is `-`). Keeping these rows preserves the porting and coverage-loss
ledger without rewriting hundreds of records. A `todo` row does not prove Rust
coverage. Mapped targets and surfaces continue to be checked against the current
Rust test lists (default and contract), Python cells and CLI/hub sources.
Dropped rows require a reason and reviewer. Strict mode also rejects todo rows
and non-todo rows without an accepted reviewer:

```sh
python3 tools/contract/mapping_check.py
TASKR_MAPPING_STRICT=1 python3 tools/contract/mapping_check.py
```

Targets are `cargo:<test binary>:<test path>`, `cell:<stem>`, or
`cell:run.py#<family>`. Surfaces name CLI commands, daemon flags, RPC routes,
spool, migration, install paths or environment variables. Subtest keys remain
unique; classes remain a, b, b-conv, c, or rust. Mapping is checked by the ordinary
Cargo `mapping_check_passes` integration test. Strict mode remains a port-completion
check and is not assumed to pass while historical todo rows remain.

## Static musl builds on this host

User-space tools are isolated in `$HOME/.cache/taskr-tools`. Installation:

```sh
python3 -m pip install --target $HOME/.cache/taskr-tools/python ziglang
mkdir -p $HOME/.cache/taskr-tools/bin $HOME/.cache/taskr-rustup
ln -s $HOME/.cache/taskr-tools/python/ziglang/zig $HOME/.cache/taskr-tools/bin/zig
# Writable mirror, because the installed rustup home is outside this lane's write scope.
cp -a --reflink=auto $HOME/.rustup/. $HOME/.cache/taskr-rustup/
export RUSTUP_HOME="$HOME/.cache/taskr-rustup"
rustup target add --toolchain stable aarch64-unknown-linux-musl
cargo install --locked --root $HOME/.cache/taskr-tools/cargo cargo-zigbuild
export PATH="$HOME/.cache/taskr-tools/bin:$HOME/.cache/taskr-tools/cargo/bin:$PATH"
# Bundled SQLite needs musl headers/compiler; Zig supplies them.
CC_x86_64_unknown_linux_musl="$PWD/tools/contract/zig-cc.sh" \
  cargo build --release -p taskr --features contract --target x86_64-unknown-linux-musl
cargo zigbuild --release -p taskr --features contract --target aarch64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/taskr target/aarch64-unknown-linux-musl/release/taskr
```

The static builds above are parity artifacts. Omit `--features contract` for production artifacts; never ship a contract build.

Versions and observed static-link results belong in the worker report; successful native tests do not prove aarch64 runtime behavior. No system packages or shell startup files are changed.
