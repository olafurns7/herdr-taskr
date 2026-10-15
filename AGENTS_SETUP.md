# First-time setup

This guide sets up taskr on a fresh machine, from nothing to a working ledger
and terminal view. It is written so an agent such as Claude Code or Codex can
follow it one step at a time, and a person can too. Every step gives the
command, what success looks like, and how to check it. Stop at the first step
that does not match and look it up in [Common failures](#8-common-failures).

It covers Linux, and macOS where it differs. The other pages go deeper:
[install.md](docs/install.md) (the install runbook, hooks, several machines,
uninstall), [daemon.md](docs/daemon.md), [cli.md](docs/cli.md),
[tui.md](docs/tui.md), [push.md](docs/push.md), [mcp.md](docs/mcp.md) and
[troubleshooting.md](docs/troubleshooting.md).

Run every command in a POSIX shell (sh, bash or zsh). An agent asks before it
changes shell profiles, agent settings or hooks, and never starts or restarts
the Herdr server from inside an agent session.

## 1. Prerequisites

```sh
uname -s; uname -m
for t in curl tar git; do command -v "$t" || echo "missing: $t"; done
command -v sha256sum || command -v shasum || echo "missing: sha256sum/shasum"
```

Expect `Linux` or `Darwin`, `x86_64`/`amd64` or `arm64`/`aarch64`, a path for
each tool and no `missing:` line. Go and Rust are not needed.

Optional, depending on how you will use taskr:

| Tool | Needed for | Check |
| --- | --- | --- |
| [Herdr](https://herdr.dev) 0.9.0 or newer | Running agent fleets; the taskr plugin starts the daemon and wakes waits | `herdr --version` |
| `gh`, signed in | Only the terminal view's GitHub panels (My PRs, Shipped, Build queue, Issues) and the hub's [PR events](docs/daemon.md#pr-events) | `gh auth status` |
| Claude Code, Codex or pi | The agents that will use taskr | `claude --version`, `codex --version`, `pi --version` |
| Tailscale | Only a [second machine](#4-host-setup) | `tailscale status` |

taskr also works without Herdr, as in [step 7](#7-verify-end-to-end), but it
is built for agents that Herdr runs.

## 2. Install taskr and taskr-tui

### Pick a version

`taskr-tui` ships from v0.21.0 on. New releases are published as GitHub
prereleases, and GitHub's "latest" release skips prereleases, so
`releases/latest` may lag behind the newest prerelease. It has pointed at
v0.18.0, which installs `taskr` without `taskr-tui` and says nothing about
it. Pin `TASKR_VERSION=v0.22.0` or newer. To find the newest
tag:

```sh
curl -fsSL 'https://api.github.com/repos/olafurns7/herdr-taskr/releases?per_page=1' |
  sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p'
```

Expect one tag, `v0.22.0` or newer. Use it as `TASKR_VERSION` below. The
[releases page](https://github.com/olafurns7/herdr-taskr/releases) lists every
tag; leaving `TASKR_VERSION` unset installs what `releases/latest` points at.

### Run the installer

Download the installer of that same tag, check it is not empty, then run it.
Do not pipe it into `sh`: a partial download could run before the failure
shows.

```sh
export TASKR_VERSION=v0.22.0   # the tag from the step above
curl -fsSL -o taskr-install.sh \
  "https://github.com/olafurns7/herdr-taskr/releases/download/$TASKR_VERSION/install.sh" &&
  test -s taskr-install.sh &&
  TASKR_LINK_SKILLS=1 sh taskr-install.sh &&
  rm taskr-install.sh
```

Expect, with your home directory in place of `~` (on a Mac the assets are
`taskr-darwin-arm64` or `taskr-darwin-amd64`):

```text
taskr-linux-amd64: OK
taskr-tui-linux-amd64: OK
skill.tar.gz: OK
plugin.tar.gz: OK
installed ~/.local/bin/taskr-tui
installed ~/.local/bin/taskr (j1 {"version":"v0.22.0","ok":true})
skill: ~/.agents/skills/taskr/SKILL.md
skill: linked: ~/.claude/skills/taskr
skill: linked: ~/.codex/skills/taskr
plugin: ~/.local/share/taskr/plugin
MANUAL: inside Herdr, run: herdr plugin link "~/.local/share/taskr/plugin"
install.sh: add ~/.local/bin to PATH
```

Every `: OK` line is a verified SHA-256 checksum; a failed one stops the
installer before anything is installed. The `skill: linked:` lines appear only
for agent config directories that already exist (`~/.claude`, `~/.codex`,
`~/.config/opencode`). Inside Herdr the `MANUAL:` line becomes
`plugin: linked olafurns7.taskr ...` or `already linked`. The last line
appears only when the install directory is not on `PATH` yet.

What lands where:

| Path | What |
| --- | --- |
| `~/.local/bin/taskr`, `~/.local/bin/taskr-tui` | The binaries (`TASKR_INSTALL_DIR` changes the directory) |
| `~/.agents/skills/taskr/` | `SKILL.md` and `references/` for agents (`TASKR_SKILL_DIR` changes it) |
| `~/.local/share/taskr/plugin/` | The Herdr plugin |
| `~/.local/state/taskr/` | The ledger and daemon state, created on first use; the installer never touches it |

Installer options (`TASKR_NO_TUI=1`, `TASKR_NO_SKILL=1`, `TASKR_NO_PLUGIN=1`
and the ones above) are in [install.md, step 2](docs/install.md#2-install).
With a signed-in `gh` the installer downloads through `gh`; otherwise through
public HTTPS.

### Put the binaries on PATH

```sh
export PATH="$HOME/.local/bin:$PATH"
command -v taskr; command -v taskr-tui; taskr version
```

Expect both paths under `~/.local/bin` and `j1 {"version":"v0.22.0","ok":true}`.
To keep it, add the `export` line to your shell profile (`~/.bashrc`,
`~/.zshrc`; on macOS usually `~/.zprofile`). Herdr's plugin looks for
`~/.local/bin/taskr`, or in `TASKR_INSTALL_DIR` when Herdr has it in its
environment.

## 3. The daemon

`taskr daemon` is one small process per user. It wakes `taskr wait` when
Herdr reports agent changes, sends owner-ask notifications through Herdr, and
serves the ledger to other machines on `127.0.0.1:7788`. Commands on the same
machine open the ledger directly; they do not need the daemon or the port.
Details: [daemon.md](docs/daemon.md).

The ledger is the SQLite file `~/.local/state/taskr/taskr.db`, created on
first use. `TASKR_DB=/path/to/file.db` makes every command use that file
instead; use it only for a scratch or test ledger, since it also overrides a
client host's `server.url`. Other settings the daemon reads:
`HERDR_SOCKET_PATH` (default `~/.config/herdr/herdr.sock`), `TASKR_TMP_BASE`
(lane tmp dirs, default `/tmp/taskr-<uid>`) and `TASKR_CHECKIN=1` (optional
[check-ins](docs/daemon.md#optional-check-ins)).

Pick one way to run it.

**Inside Herdr (the usual case).** Link the plugin once; it then starts the
daemon at every Herdr start and agent detection:

```sh
herdr plugin list --json | grep -q '"olafurns7.taskr"' ||
  herdr plugin link "$HOME/.local/share/taskr/plugin"
taskr daemon --restart   # start it now instead of at the next agent detection
```

**Linux, at boot.** Use the systemd user unit in
[install.md, step 4](docs/install.md#start-the-ledger-host-at-boot-linux)
(`ExecStart=... taskr daemon --stay`, `loginctl enable-linger`). It keeps the
ledger served while Herdr is down.

**macOS, at login.** taskr ships no launchd agent. The one below is the same
idea as the systemd unit. Run `mkdir -p ~/Library/LaunchAgents`, save it as
`~/Library/LaunchAgents/dev.taskr.daemon.plist` with your home directory in
place of `/Users/you`, then run
`launchctl load ~/Library/LaunchAgents/dev.taskr.daemon.plist`. The
[health check](#health-check) below must pass before you rely on it.

Untested on macOS:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.taskr.daemon</string>
  <key>ProgramArguments</key><array>
    <string>/Users/you/.local/bin/taskr</string><string>daemon</string><string>--stay</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>/Users/you/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict></plist>
```

**In the foreground, without Herdr.** Plain `taskr daemon` exits at once when
there is no Herdr socket (its log says `exit: socket removed`), so use
`--stay`:

```sh
taskr daemon --stay   # Ctrl-C stops it; or run it in another terminal or tab
```

### Health check

```sh
taskr daemon --status
```

Expect one `j1 {...}` line with `"running":true`, `"running_version"` equal to
`taskr version`, `"stale":false`, `"dashboard":"up"` and
`"dashboard_url":"http://127.0.0.1:7788/"` (the field names are historical; no
web page is served there, so a browser gets 404). Just after a start the
status can read `"dashboard":"down"`, `"running_version":"unknown"` and
`"stale":true` for a moment; run it again. Also expect `"daemon":"fresh"`
inside Herdr; without Herdr it is `"none"` with `"herdr_missing":true`, which
is fine for a ledger-only setup. A zero exit alone does not mean a daemon is
running: read `running`. The log is `~/.local/state/taskr/daemon.log`.

## 4. Host setup

**One machine:** nothing to do. It keeps its own ledger and is `"role":"local"`
in the status.

**A second machine (optional):** one host keeps the ledger (the hub); the
others send every command to it over Tailscale. Both must be untagged
Tailscale nodes of the same Tailscale user.

1. On the hub: `mkdir -p ~/.local/state/taskr && echo tailnet > ~/.local/state/taskr/dashboard.addr`, then
   `taskr daemon --restart`. Expect `"role":"hub"` and a `"tailnet_url"` in
   `taskr daemon --status`.
2. On the client: install taskr (step 2) and leave `TASKR_DB` unset. Write
   the hub's `tailnet_url` to `server.url`; the state directory does not
   exist until taskr first runs, so create it:

   ```sh
   mkdir -p ~/.local/state/taskr
   printf '%s\n' 'http://hub.example.ts.net:7788' > ~/.local/state/taskr/server.url   # your hub's tailnet_url
   taskr daemon --restart
   ```

   Expect `"mode":"client"` with the `"server"` in `taskr daemon --status`,
   and `taskr status` answering from the hub.

Full steps, the spool and moving an existing ledger:
[install.md, step 7](docs/install.md#7-multiple-machines) and
[daemon.md](docs/daemon.md#three-kinds-of-host).

## 5. Agent integration

### The skill

Agents learn the commands from the skill, not from these docs. The installer
put it in `~/.agents/skills/taskr/` (`SKILL.md` plus `references/format.md`,
`orchestrator.md`, `recovery.md` and `review.md`) and, with
`TASKR_LINK_SKILLS=1`, linked it from each existing agent config:

```sh
ls ~/.agents/skills/taskr ~/.agents/skills/taskr/references
for d in ~/.claude ~/.codex ~/.config/opencode; do
  if [ -d "$d" ]; then echo "$d/skills/taskr -> $(readlink "$d/skills/taskr")"; fi
done
```

Expect `SKILL.md`, `references` and the four reference files, and each link
pointing at `~/.agents/skills/taskr`. pi reads `~/.agents/skills` itself. An
agent that started before the install sees the skill in its next session.
Tell an orchestrating agent to load the taskr skill before it delegates work.

If you installed Claude Code or Codex after taskr, rerun the installer with
`TASKR_LINK_SKILLS=1`, or link by hand:

```sh
mkdir -p ~/.claude/skills && ln -s ~/.agents/skills/taskr ~/.claude/skills/taskr
```

### Hooks (optional)

Hooks let taskr record prompt receipts and notice a lane that stopped while it
still owed a report. Nothing installs them for you. The entries for Claude
Code, Codex, OpenCode and pi are in
[install.md, step 6](docs/install.md#6-agent-hooks); merge them into your
existing settings, never replace the file.

### The MCP server and push channel

Without any of this, agents use taskr through the CLI and `taskr wait`. A push
adapter wakes an idle agent when an event arrives instead. All of them are
opt-in and need a lane identity (`TASKR_TASK` and `TASKR_LAUNCH`, which the
orchestrator sets when it starts a worker) or, for a root, `--as ROOT
--session GEN`. Without one, `taskr mcp` exits 2. What each promises and what
is still unproved: [push.md](docs/push.md).

**Claude Code** (research preview). Write the server to a file, then start the
session with the development-channel flag:

```sh
cat > taskr-mcp.json <<'EOF'
{"mcpServers": {"taskr": {"command": "taskr", "args": ["mcp", "--channel"]}}}
EOF
claude --mcp-config taskr-mcp.json --dangerously-load-development-channels server:taskr
```

A project `.mcp.json` with the same content works too. Use `"args": ["mcp"]`
for the tools without pushes. The caveats from
[push.md](docs/push.md#claude-code-research-preview): Claude Code 2.1.296 did
not register a server given only with `--mcp-config` as a channel (2.1.300
does), the flag shows a confirmation dialog at every start, it is ignored with
`-p` and the SDK, it needs claude.ai or Console API-key sign-in, and Team or
Enterprise organizations must set `channelsEnabled`. A session that is not
registered as a channel drops pushes silently, so accept a new Claude version
only after one push was received and acked.

**Codex.** Codex gets pushes from a sidecar, `taskr push codex`, run in its
own tab beside the session after `taskr start` registered the thread:
[push.md, Codex](docs/push.md#codex).

**pi.** pi loads the push extension from a clone of this repository:
`pi --extension /path/to/herdr-taskr/adapters/pi/taskr-push.ts`
([push.md, pi](docs/push.md#pi)).

The check in [step 7](#7-verify-end-to-end) starts the server for a
throwaway lane, sends it the MCP handshake and lists its nine tools.

## 6. Terminal view settings

`taskr-tui` keeps its choices in `~/.config/taskr/tui.json`
(`$XDG_CONFIG_HOME/taskr/tui.json` when that is set). With no file it uses
the defaults. The GitHub settings:

| Key | Value | Unset |
| --- | --- | --- |
| `"repos"` | A list of `"owner/name"`, at most eight. My PRs, Shipped and Build queue read these. | Empty: those panels are hidden and make no `gh` calls. |
| `"issues_repo"` | One `"owner/name"` for the Issues panel. | The Issues panel is hidden and makes no `gh` calls. |
| `"health_repo"` | One `"owner/name"`. The Health panel shows the hub's latest `HEALTH repo=... env=...` note for it, read from the ledger, not from GitHub. Needs `"health_root"` too. | The Health panel is hidden. |
| `"health_root"` | A positive integer: the hub root task whose `HEALTH` notes the Health panel reads. Needs `"health_repo"` too. | The Health panel is hidden and runs no `taskr log`. |

These need a `taskr-tui` newer than v0.22.0 (`"health_root"`: newer than v0.22.1). v0.22.0 reads only
`"panels"` and `"repos"`, falls back to a built-in repository when `"repos"`
is missing, and drops the other two keys when a panel toggle saves the file.
An example:

```json
{
  "repos": ["example-org/app", "example-org/api"],
  "issues_repo": "example-org/app",
  "health_repo": "example-org/app",
  "health_root": 4242
}
```

On the Dashboard, the keys `1`-`9`, `0` and `H` show or hide a panel and save
the choice to the same file, as `"panels"`.
Deleting the file restores the defaults. A file that does not parse is left
alone: the view shows `TUI config invalid; using defaults`, and toggles then
last only for the session until you fix it. The GitHub panels use the `gh`
login of the machine running `taskr-tui`. Every panel and key:
[tui.md](docs/tui.md).

## 7. Verify end to end

This checks the task flow: it registers a small campaign, reports through it
as a worker would, and reads it back. It needs no Herdr and no agent. It runs
on a throwaway ledger, `~/taskr-check/check.db`, never your real one: the
script runs in a subshell that sets `TASKR_DB` and unsets `TASKR_TASK`,
`TASKR_LAUNCH` and `TASKR_BIN`. That way it is safe inside an agent lane or on
a client host with `server.url`, and your environment is unchanged
afterwards. The daemon's health is checked separately, by
[step 3](#health-check).

`mkdir` stops the script if `~/taskr-check` already exists; move that
directory aside first or pick another name.

```sh
mkdir ~/taskr-check && (
  cd ~/taskr-check || exit 1
  unset TASKR_TASK TASKR_LAUNCH TASKR_BIN
  export TASKR_DB="$PWD/check.db"
  set -e
  root=$(taskr new setup-check --role orchestrator | sed -n 's/^n1 //p'); echo "root=$root"
  printf 'Check the taskr install.\n' > brief.md
  lane=$(taskr new setup-lane --role implementer --parent "$root" --brief "$PWD/brief.md" | sed -n 's/^n1 //p')
  launch=$(taskr launch "$lane" --provider manual --model none --effort none |
    sed -n 's/.*"launch_id":\([0-9]*\).*/\1/p')
  echo "lane=$lane launch=$launch"
  # The worker reports (an orchestrator would start an agent with these set):
  TASKR_TASK=$lane TASKR_LAUNCH=$launch taskr note "install checked"
  printf 'All good.\n' > report.md
  TASKR_TASK=$lane TASKR_LAUNCH=$launch taskr ready "setup check done" --report "$PWD/report.md"
  # The orchestrator wakes on the report and marks it handled:
  event=$(taskr wait --as "$root" --for ready --timeout 5000 | tee /dev/stderr | cut -f2)
  taskr ack "$event" --as "$root"
  # The owner's view:
  taskr glance --brief
  # The MCP server, bound to the lane (step 5):
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"check","version":"0"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' |
    TASKR_TASK="$lane" TASKR_LAUNCH="$launch" taskr mcp | grep -o '"name":"taskr_[a-z]*"'
)
```

Expect, in order:

- on stderr, two notes that are normal for a root made by hand:
  `no goal recorded for root 1` and `lead has no pane`;
- `root=1`, then `lane=2 launch=1`;
- `n1 1`, then `r1 2` (the note and the ready report);
- an `e1` line ending in `"setup check done"` and the report path, then
  `k1 2`;
- a header line `taskr · <time> · cursor=2 · 1 to check`, a `⚠ hub  daemon`
  line ("no live Herdr connection; no heartbeat": this throwaway ledger has
  never seen a daemon, so it says so even when yours is healthy), then
  `○ setup-check  root=1  0/1 working ...` with `setup check done` under it;
- nine tool names, `"name":"taskr_task"` through `"name":"taskr_fail"`.

Then open the terminal view in a real terminal, first with invented data,
then on the throwaway ledger:

```sh
taskr-tui --demo   # invented data; reads and writes nothing
env -u TASKR_TASK -u TASKR_LAUNCH -u TASKR_BIN TASKR_DB="$HOME/taskr-check/check.db" taskr-tui
```

Expect `setup-check` under CAMPAIGNS in the second. Press `?` for the keys and
`q` to quit. `taskr-tui` without the `env` prefix shows your real ledger.

Finally remove only what the check created:

```sh
rm -f ~/taskr-check/brief.md ~/taskr-check/report.md \
  ~/taskr-check/check.db ~/taskr-check/check.db-wal ~/taskr-check/check.db-shm
rmdir ~/taskr-check
```

`rmdir` fails, and keeps the directory, if anything else is in it.

## 8. Common failures

Start with `taskr version`, `taskr daemon --status` and
`~/.local/state/taskr/daemon.log`. More cases are in
[troubleshooting.md](docs/troubleshooting.md); agents also have
`~/.agents/skills/taskr/references/recovery.md`.

| Symptom | Cause and fix |
| --- | --- |
| `taskr: command not found` | `~/.local/bin` is not on `PATH`: [step 2](#put-the-binaries-on-path). The installer's last line says so. |
| `taskr version` shows v0.18.0 or another release before v0.21.0; no `taskr-tui` | You installed from `releases/latest`, which skips prereleases. Reinstall with `TASKR_VERSION=v0.22.0` or newer ([step 2](#pick-a-version)). |
| `install.sh: could not stage taskr` | The install directory is not writable. Use the default `~/.local/bin` or a `TASKR_INSTALL_DIR` you own; do not run the installer with sudo. |
| `checksum verification failed` or `could not download release asset` | A partial or blocked download, or a tag without that asset. Check the tag on the releases page and run again; nothing was installed. |
| `skill: conflict: ...` | An existing skill link or directory the installer will not overwrite. Inspect it and replace it yourself only if it is an old taskr. |
| `"running":false` | No daemon. Inside Herdr: `taskr daemon --restart`, and check the plugin is linked (`herdr plugin list --json`). Outside Herdr: `taskr daemon --stay` ([step 3](#3-the-daemon)). |
| `taskr daemon` returns at once; the log says `exit: socket removed` | No Herdr socket. Use `taskr daemon --stay`, or run Herdr. |
| `taskr daemon --restart` exits 6: `the new daemon ... exited at start` | Outside Herdr with no daemon running, `--restart` starts a plain daemon, which exits. Start `taskr daemon --stay`; once one runs, `--restart` keeps its `--stay`. |
| `"stale":true` | The daemon is older than the binary, usually after an upgrade: `taskr daemon --restart`. |
| `"herdr_missing":true` | The daemon cannot find `herdr` on its `PATH`, common under systemd or launchd: put Herdr's directory in the unit's `PATH`. Harmless without Herdr. |
| `"dashboard":"down"`; log says `listen 127.0.0.1:7788 failed: Address in use` | Another process has port 7788. Commands on this machine still work. For a hub, write `127.0.0.1:PORT` or `tailnet:PORT` to `~/.local/state/taskr/dashboard.addr` and restart ([troubleshooting.md](docs/troubleshooting.md#port-7788-is-taken)). |
| A command exits 5, `server unreachable` | A client cannot reach the hub. Check `server.url`, `tailscale status`, and the hub's `taskr daemon --status` ([troubleshooting.md](docs/troubleshooting.md#the-hub-is-unreachable-from-a-client)). |
| Exit 2, `server.url refused: ... is not a MagicDNS name under ...` | `server.url` must name a host of your own tailnet. Copy the hub's `tailnet_url` exactly. |
| A write printed `qd1 <key>` | Queued on a client while the hub was unreachable; exit 0. Do not run it again: `taskr spool ls`. |
| Exit 6 | A refusal (closed task, stale launch, wrong host, or an inbox held by a push adapter). Do not retry; read the message. |
| A Claude session never gets pushes | Not registered as a channel: no development-channel flag, `-p`, an older Claude with `--mcp-config` only, or an organization without `channelsEnabled`. `taskr delivery ls --open` lists pushes with no receipt ([push.md](docs/push.md#claude-code-research-preview)). |
| `mcp needs TASKR_TASK and TASKR_LAUNCH ...` (exit 2) | `taskr mcp` was started outside a lane. Configure it only for sessions launched as a lane, or as a root with `--as ROOT --session GEN`. |
| `taskr-tui: needs a terminal` (exit 2) | It was run in a pipe or script. Run it in a terminal, or use `taskr glance`. |
| My PRs says `unavailable: your GitHub login could not be read`; Shipped or Build queue say `unavailable; authentication required` | `gh` is missing or not signed in on this machine: `gh auth login`, then `gh auth status`. |
| GitHub panels are missing or empty | No `"repos"` (or `"issues_repo"`) in `tui.json`, or the panel is switched off: [step 6](#6-terminal-view-settings). An empty search says `0 observed`, not an error. |
| The view says `No listed campaigns` or `No open campaigns` | Not an error: the ledger has nothing open. Try `taskr-tui --demo`. |
| `TUI config invalid; using defaults` | `tui.json` does not parse or has a bad repository name. Fix or delete it, then restart the view. |
