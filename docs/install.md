# Installing taskr

Start with the [README](../README.md). These instructions assume a normal
human-operated shell, with Herdr already installed and running for plugin
commands. Do not start or restart the Herdr server from an agent session.

## Installer options

`install.sh` accepts environment variables, not command-line flags:

| Variable | Effect |
| --- | --- |
| `TASKR_VERSION` | Release tag; unset selects the latest release. |
| `TASKR_INSTALL_DIR` | Binary directory; default `$HOME/.local/bin`. |
| `TASKR_SKILL_DIR` | Skill directory; default `$HOME/.agents/skills/taskr`. |
| `TASKR_NO_SKILL=1` | Skip `skill.tar.gz`. |
| `TASKR_LINK_SKILLS=1` | Link skills into existing agent configuration directories. |
| `TASKR_NO_PLUGIN=1` | Skip `plugin.tar.gz` and plugin linking. |
| `GH_TOKEN` / `GITHUB_TOKEN` | Authenticate downloads when an authenticated `gh` is unavailable. |

The installer downloads `taskr-OS-ARCH`, `SHA256SUMS`, `skill.tar.gz`, and
`plugin.tar.gz`, verifies selected assets, then copies them into place.
It prefers authenticated `gh release download`, then GitHub's release API
with a token. Without either, it downloads assets anonymously with `curl`
from `https://github.com/olafurns7/herdr-taskr/releases/latest/download/<asset>`.
With `TASKR_VERSION` set, the URL is
`https://github.com/olafurns7/herdr-taskr/releases/download/<tag>/<asset>`.
All paths verify the selected assets against `SHA256SUMS`.

Skill links are opt-in, only for existing `$HOME/.claude`, `$HOME/.codex`, and
`$HOME/.config/opencode` directories. Conflicting links, files, or directories
are reported and left intact. The plugin launcher honors `TASKR_INSTALL_DIR`,
falling back to `$HOME/.local/bin/taskr`. For a custom binary directory, keep
`TASKR_INSTALL_DIR` set in Herdr's plugin environment as well as during install.

## Public release downloads

The primary curl installer command is in the [README](../README.md#install).
The first public release is v0.11.3 on `master`; the examples select the latest
release so they continue to work after updates. Development takes place in
[the public repository](https://github.com/olafurns7/herdr-taskr).

Release assets are `install.sh`, `SKILL.md`, `SHA256SUMS`, `skill.tar.gz`,
`plugin.tar.gz`, and four binaries: `taskr-darwin-arm64`, `taskr-darwin-amd64`,
`taskr-linux-arm64`, and `taskr-linux-amd64`. Go and Node are not needed for
installation. The installer determines your OS and architecture and checks
that the skill archive contains `SKILL.md` and `references/`.

To select a release, set `TASKR_VERSION` on the shell running the installer:

```sh
# Use the latest installer to install the first public release's payloads.
curl -fsSL -o taskr-install.sh https://github.com/olafurns7/herdr-taskr/releases/latest/download/install.sh &&
test -s taskr-install.sh &&
TASKR_VERSION=v0.11.3 TASKR_LINK_SKILLS=1 sh taskr-install.sh
```

## Agent hooks

The installer provides the taskr hook receiver, but no harness configuration.
The Herdr plugin observes panes; it does not install agent hooks.
Merge entries into your existing hook configuration rather than replacing it.

| Harness argument | Events accepted by taskr |
| --- | --- |
| `claude` | `SessionStart`, `UserPromptSubmit`, `Stop`, `StopFailure` |
| `codex` | `SessionStart`, `UserPromptSubmit`, `Stop` |
| `opencode` | `session.created`, `chat.message`, `session.idle`, `session.error` |

For each Claude Code or Codex event, use this command template, substituting
the harness argument and event name from the table:

```sh
# Example command registered for Claude Code's UserPromptSubmit event.
[ -n "${TASKR_LAUNCH:-}" ] && taskr hook claude UserPromptSubmit >/dev/null 2>&1; true
```

For example, merge this entry into Claude Code's `settings.json`, and add the
other supported events with their corresponding command arguments:

```json
{
  "hooks": {
    "UserPromptSubmit": [{
      "hooks": [{
        "type": "command",
        "command": "[ -n \"${TASKR_LAUNCH:-}\" ] && taskr hook claude UserPromptSubmit >/dev/null 2>&1; true",
        "async": true
      }]
    }]
  }
}
```

For Codex, register the same command entries in `hooks.json`, using `codex`
and its supported events. Use your installed harness's configuration format.
For OpenCode, a plugin must invoke `taskr hook opencode EVENT`, write one JSON
object to its stdin, and close stdin. Forward event objects for `session.*`;
for `chat.message`, forward `{"input": input, "output": output}`. No OpenCode
hook plugin is shipped here.

The receiver acts only with `HERDR_ENV=1` and a registered `TASKR_LAUNCH`.
It exits silently, gives up after 500 ms, and never creates a ledger file.
Configure hooks before starting the worker so its initial session is bound.
The worker still explicitly acknowledges prompts and reports its result.
See the [recovery reference](../references/recovery.md) for handling stalls.

## Multiple machines

Tailscale must already be running on every participating machine. Use untagged
nodes signed in to the same Tailscale user. taskr checks node identity with
`tailscale whois`; tagged nodes and nodes belonging to other users are refused.
Traffic is HTTP over the encrypted tailnet, without an additional taskr token.
Replace `hub.example.ts.net` with your hub's actual MagicDNS name.

### Hub/server

On the machine that will hold the shared ledger:

```sh
# Enable loopback and Tailscale dashboard listeners on port 7788.
mkdir -p "$HOME/.local/state/taskr"
printf 'tailnet\n' > "$HOME/.local/state/taskr/dashboard.addr"
# Apply the configuration to the taskr daemon.
taskr daemon --restart
# Inspect listener and daemon state.
taskr daemon --status
```

The hub serves the dashboard and `/api/rpc`. Its own CLI uses the local ledger;
it should not have `server.url`. RPC clients must be other admitted tailnet
nodes: loopback and the server's own node are refused by the RPC endpoint.

### Fresh client

On another machine with no ledger to migrate:

```sh
# Route ledger commands to the shared server.
mkdir -p "$HOME/.local/state/taskr"
printf 'http://hub.example.ts.net:7788\n' > "$HOME/.local/state/taskr/server.url"
# Switch the taskr daemon to client mode and inspect it.
taskr daemon --restart
taskr version
taskr daemon --status
```

Leave `TASKR_DB` unset: setting it forces local mode. The client daemon reports
its local panes to the server. Ledger commands open no local ledger and fail
with exit 5 when the server is unreachable. For a write, rerun the exact
`retry with:` command printed by taskr, retaining its request key.

Moving an existing local ledger to client mode needs a backup and a deliberate
migration; writing `server.url` does not import old tasks. See
[recovery](../references/recovery.md) before changing an existing setup.

### Separate-ledger peer

If you want a combined dashboard but independent local ledgers, configure
`hub.url` instead of `server.url`:

```sh
# Send this machine's dashboard snapshots to the hub.
mkdir -p "$HOME/.local/state/taskr"
printf 'http://hub.example.ts.net:7788\n' > "$HOME/.local/state/taskr/hub.url"
# Apply peer configuration and inspect push health.
taskr daemon --restart
taskr daemon --status
```

This requires no `server.url`. The hub combines read-only snapshots; each
machine's commands still use that machine's ledger. Answer owner questions
on their originating machine. Do not set up both modes as though they were
interchangeable.

## Upgrades

Rerun your chosen install procedure, then run `taskr daemon --restart` from a
human-operated shell to replace the taskr daemon. It refuses to signal a
process whose identity it cannot verify. See [recovery](../references/recovery.md)
if an old daemon cannot be restarted. This does not restart Herdr.

## Uninstall

Inside the running Herdr session, unlink the taskr plugin first so it cannot
bring the taskr daemon back:

```sh
# Remove the plugin registration in Herdr.
herdr plugin unlink olafurns7.taskr
# Find the taskr daemon PID before stopping it.
taskr daemon --status
```

Stop that PID from a human-operated terminal (or press Ctrl-C if the daemon
is running in the foreground). Then remove installed files:

```sh
# Remove only symlinks targeting the default installed skill.
for link in "$HOME/.claude/skills/taskr" "$HOME/.codex/skills/taskr" "$HOME/.config/opencode/skills/taskr"; do
  if [ -L "$link" ] && [ "$(readlink "$link")" = "$HOME/.agents/skills/taskr" ]; then
    rm "$link"
  fi
done
# Remove the default binary, plugin files, and installed agent docs.
rm -f "$HOME/.local/bin/taskr"
rm -rf "$HOME/.local/share/taskr/plugin" "$HOME/.agents/skills/taskr"
```

For custom install paths, remove those paths and their links instead.
This leaves `$HOME/.local/state/taskr` (including the ledger) and all report
files intact. Remove that state directory separately only when you intend to
discard its history and configuration.
