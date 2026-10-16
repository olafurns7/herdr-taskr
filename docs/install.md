# Install taskr for Herdr

This runbook is written so an agent can follow it; every step is plain shell, so a person can too. Other pages: [the terminal view](tui.md), [the commands](cli.md), [the daemon and the hub](daemon.md), [troubleshooting](troubleshooting.md).

Follow these steps in order for installation; for uninstall-only requests, go directly to step 9. Stop on failure; report the step, command, exit code, and expected result. Do not continue with a partial install.
Never start or restart the Herdr server from an agent session. Ask before changing any hook or agent settings; preserve existing settings and conflicting skill links.

## 1. Check preflight

Verify inside an already-running Herdr session:
```sh
uname -s; uname -m
herdr --version
if command -v curl >/dev/null; then curl -fsSL https://raw.githubusercontent.com/olafurns7/herdr-taskr/master/plugin/herdr-plugin.toml; else gh api repos/olafurns7/herdr-taskr/contents/plugin/herdr-plugin.toml -H 'Accept: application/vnd.github.raw+json'; fi | sed -n '/^min_herdr_version/p'
command -v curl || command -v gh
command -v tar
command -v shasum || command -v sha256sum
```
Expect Darwin or Linux, arm64/aarch64 or x86_64/amd64, and all tools found. Compare Herdr's version numerically against the fetched manifest's floor. If Herdr is missing, too old, or the floor cannot be fetched, stop and tell the user. Go is unnecessary.

## 2. Install

Pin the release. `taskr-tui` ships from v0.21.0 on, and new releases start as GitHub prereleases. `releases/latest` skips prereleases, so it may lag behind the newest prerelease; it has installed v0.18.0, without `taskr-tui` and without saying so. Use `TASKR_VERSION=v0.22.0` or newer; `curl -fsSL 'https://api.github.com/repos/olafurns7/herdr-taskr/releases?per_page=1' | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p'` prints the newest tag.

Use curl without signing in. Do not pipe to sh: a partial download could execute before failure is reported.
```sh
export TASKR_VERSION=v0.22.0   # or a newer tag
curl -fsSL -o taskr-install.sh \
  "https://github.com/olafurns7/herdr-taskr/releases/download/$TASKR_VERSION/install.sh" &&
  test -s taskr-install.sh &&
  TASKR_LINK_SKILLS=1 sh taskr-install.sh &&
  PATH="$HOME/.local/bin:$PATH" taskr version
```
Alternatively, use GitHub CLI:
```sh
# Download the installer through GitHub CLI, then install and link agent skills.
export TASKR_VERSION=v0.22.0   # or a newer tag
gh api "repos/olafurns7/herdr-taskr/contents/install.sh?ref=$TASKR_VERSION" \
  -H 'Accept: application/vnd.github.raw+json' > taskr-install.sh &&
  test -s taskr-install.sh &&
  TASKR_LINK_SKILLS=1 sh taskr-install.sh &&
  PATH="$HOME/.local/bin:$PATH" taskr version
```
Then remove taskr-install.sh.
Expect checksum verification for the binary, the `taskr-tui` binary, the skill archive, and the plugin archive, then the pinned release version. On checksum or download failure, stop; do not run the partial installer.

| Option | Effect |
| --- | --- |
| `TASKR_INSTALL_DIR` | Binary directory; default `$HOME/.local/bin`. Export it before installing; use that directory in PATH and Herdr's plugin environment. |
| `TASKR_VERSION` | Release tag; unset selects `releases/latest`, which skips prereleases and may lag behind the newest one (it has given v0.18.0, without `taskr-tui`). A prerelease installs only when `TASKR_VERSION` names its tag. |
| `TASKR_LINK_SKILLS=1` | Link the installed skill into existing agent config directories; preserve conflicts. |
| `TASKR_HERDR_CONFIG=1` | Inside Herdr, add missing sidebar keys to herdr-radar's config.toml after `herdr plugin config-dir hhdebb.herdr-radar` succeeds; preserve existing values. Default: print manual instructions without editing. See [sidebar settings](daemon.md#pane-and-workspace-tokens). |
| `TASKR_SKILL_DIR` | Skill directory; default `$HOME/.agents/skills/taskr`. |
| `TASKR_NO_SKILL=1` / `TASKR_NO_PLUGIN=1` | Skip the corresponding archive and setup. |

The sidebar step leaves symlinked configs and complex TOML (multiline strings or escaped root keys) for manual setup, without changing the file.

## 3. Set PATH

If the install directory is absent from PATH, add it for this session:
```sh
export PATH="${TASKR_INSTALL_DIR:-$HOME/.local/bin}:$PATH"
command -v taskr; taskr version
```
Expect the installed binary and the release version. Persist the PATH line in the user's shell config only if requested.

## 4. Link the plugin and check the daemon

The installer may already have linked the plugin. Run inside Herdr:
```sh
herdr plugin list --json | grep -q '"olafurns7.taskr"' || herdr plugin link "$HOME/.local/share/taskr/plugin"
taskr version
taskr daemon --status
```
Expect plugin registration and daemon fields `running: true`, the installed `running_version`, and `dashboard: up` with `dashboard_url`, normally http://127.0.0.1:7788/. This is the address of the hub server, which other hosts' commands call; the field names are historical and no web page is served there. A successful status exit alone does not prove the daemon is running.
If status does not show `running: true`, run `taskr daemon --restart`, then re-check with `taskr daemon --status`. It starts the daemon detached with only HOME, PATH, and HERDR_SOCKET_PATH; a fresh plugin link otherwise waits until the next agent detection or Herdr start. For upgrades, re-run step 2, then `taskr daemon --restart` and re-check status. This restarts taskr, never Herdr.

### Start the ledger host at boot (Linux)

On the ledger's host, create `~/.config/systemd/user/taskr.service`:
```ini
[Unit]
Description=taskr ledger

[Service]
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin
ExecStart=%h/.local/bin/taskr daemon --stay
Restart=always
RestartPreventExitStatus=2
RestartSec=2

[Install]
WantedBy=default.target
```
Use the installed binary's path if it differs. Enable the unit with
`systemctl --user enable --now taskr.service`. Enable lingering for that user
(`loginctl enable-linger`) so the user service starts at boot without a login.
The PATH must hold `herdr` and `tailscale` because a user service manager does
not read the shell profile; `Environment=HERDR_SOCKET_PATH=...` is needed only
when Herdr does not use the default socket, `$HOME/.config/herdr/herdr.sock`.
`--stay` is for the local ledger host; it keeps the hub server available
while Herdr is down and reconnects when Herdr returns. It waits for an existing
daemon's lock and retries an unavailable listener.
If a daemon started by the plugin holds the lock when the unit is first
enabled, get its pid from `taskr daemon --status` and end it with `kill <pid>`;
the unit's waiting daemon then takes over.
Herdr's plugin hook still starts a daemon on hosts without this unit.

## 5. Verify skills

The installer copies SKILL.md and references/ to `$HOME/.agents/skills/taskr` (or TASKR_SKILL_DIR). With TASKR_LINK_SKILLS=1 it links only existing `$HOME/.claude`, `$HOME/.codex`, and `$HOME/.config/opencode` directories:
```sh
skill_dir=${TASKR_SKILL_DIR:-$HOME/.agents/skills/taskr}
test -f "$skill_dir/SKILL.md" && test -d "$skill_dir/references"
for config in "$HOME/.claude" "$HOME/.codex" "$HOME/.config/opencode"; do
  if [ -d "$config" ]; then readlink "$config/skills/taskr" && test "$config/skills/taskr" -ef "$skill_dir" || exit 1; fi
done
```
Expect the installed skill and each applicable link to resolve to it. If configs do not exist, report no links; if a conflict exists, report it and leave it unchanged. pi reads `$HOME/.agents/skills` itself and needs no link. Tell the orchestrator agent to load the installed taskr skill before coordinating workers.

## 6. Agent hooks

ASK the user before any hook or settings edit. Without approval, skip hooks and report them disabled. Neither the installer nor the Herdr plugin configures hooks.
After approval, merge command entries into existing Claude Code settings.json or Codex hooks.json using the installed harness's format; never replace the file. Use `claude` with SessionStart, UserPromptSubmit, Stop, StopFailure; `codex` with SessionStart, UserPromptSubmit, Stop. Template for each event:
```sh
[ -n "${TASKR_LAUNCH:-}" ] && taskr hook claude UserPromptSubmit >/dev/null 2>&1; true
```
For Claude Code, an entry is `{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"[ -n \"${TASKR_LAUNCH:-}\" ] && taskr hook claude UserPromptSubmit >/dev/null 2>&1; true","async":true}]}]}}`; add the other events with matching arguments.
For Codex, an entry is `{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"[ -n \"${TASKR_LAUNCH:-}\" ] && taskr hook codex UserPromptSubmit >/dev/null 2>&1; true","timeout":2}]}]}}`; add SessionStart and Stop with matching arguments. Codex runs these synchronously with a two-second timeout; Claude uses `async:true`. Use a quoted absolute binary path in place of `taskr` if the harness PATH does not contain your install directory.

OpenCode and pi hook adapters ship in this repository; the installer does not copy them. From the root of a trusted checkout of the taskr release you installed, after approval, copy the adapter for the harness you use. These commands stop if a destination already exists; inspect and merge an existing adapter instead of overwriting it.

OpenCode:
```sh
mkdir -p "$HOME/.config/opencode/plugins" &&
  test ! -e "$HOME/.config/opencode/plugins/taskr-hook.js" &&
  test ! -L "$HOME/.config/opencode/plugins/taskr-hook.js" &&
  cp adapters/opencode/taskr-hook.js "$HOME/.config/opencode/plugins/taskr-hook.js"
```
The plugin forwards session.created/session.idle/session.error objects to `taskr hook opencode EVENT`; chat.message sends `{"input": input, "output": output}`.

pi:
```sh
mkdir -p "$HOME/.pi/agent/extensions" &&
  test ! -e "$HOME/.pi/agent/extensions/taskr-hook.ts" &&
  test ! -L "$HOME/.pi/agent/extensions/taskr-hook.ts" &&
  cp adapters/pi/taskr-hook.ts "$HOME/.pi/agent/extensions/taskr-hook.ts"
```
The extension forwards session_start, input, and agent_settled to `taskr hook pi EVENT`. Each payload contains its type, available sessionId, and sessionFile only when absolute; it adds reason for session_start, text/source for input, and the branch's last assistant message reduced to stopReason and diagnostic type/error name/code for agent_settled. An error stopReason records an error stall.

Both adapters run only when TASKR_LAUNCH is set, write JSON to the child's stdin and close it, spawn detached, and swallow delivery errors. They use `$HOME/.local/bin/taskr`; if you installed elsewhere, change that path in the copied adapter. Start a new harness session to load the copied plugin/extension. pi's hook extension is separate from the [push extension](push.md#pi); either can be used independently.
Verify `taskr hook --help` succeeds and inspect the merged entries against the event list. Expect valid harness config; runtime receipts require HERDR_ENV=1 and a registered TASKR_LAUNCH, configured before worker startup. Workers still run got and ready/done/fail explicitly. A receipt is acknowledgment, not completion; a stall is a signal to investigate.

## 7. Multiple machines

Only configure this if the user asks. Require running Tailscale with untagged nodes owned by the same user; identity checks reject tagged nodes and other users. HTTP traffic stays inside the encrypted tailnet.
On the taskr hub (the host that keeps the ledger), write `tailnet` to `$HOME/.local/state/taskr/dashboard.addr`; the file name is historical and sets where the hub server listens. On a fresh client, write the hub URL (for example http://hub.example.ts.net:7788) to `server.url` in that directory; leave TASKR_DB unset. There is no local fallback when the hub is unreachable.
`hub.url` belonged to an older setup with one ledger per machine; nothing reads it now except `daemon --status`, so do not write it. Before migrating an existing ledger, back it up and read https://raw.githubusercontent.com/olafurns7/herdr-taskr/master/references/recovery.md.
Client records queue in `$HOME/.local/state/taskr/spool/` when the server is unreachable or earlier records wait there; a running client daemon sends them after a pass that reaches the server, or use `taskr spool send` on that host. Without a daemon they stay queued until manual send; a full or unwritable spool exits 5 with `retry with:` (rerun that line). `qd1 <request key>` means queued, exit 0: do not retry. Check `taskr spool ls` after an outage.

Apply changes with `taskr daemon --restart`; verify `taskr daemon --status` shows the selected mode and listeners (`role: hub` with a `tailnet_url` on the hub; `mode: client` with the `server` on a client) and a fresh connection. On write exit 5, rerun the exact printed `retry with:` command, preserving its request key.

## 8. The terminal view (optional)

`taskr-tui` is the owner's view of the ledger. `install.sh` from v0.21.0 on installs it beside `taskr`; set `TASKR_NO_TUI=1` to opt out. An older release, such as v0.18.0 from `releases/latest`, installs no `taskr-tui`: reinstall with `TASKR_VERSION` pinned (step 2). Check it with:
```sh
taskr-tui --demo
```
Expect a full-screen view of invented data; `?` lists the keys and `q` quits. `taskr-tui` without `--demo` shows the user's ledger. See [tui.md](tui.md).

To build from source instead, with a Rust toolchain:
```sh
git clone https://github.com/olafurns7/herdr-taskr && cd herdr-taskr
cargo install --path crates/taskr-tui
```

## 9. Uninstall

Only uninstall if requested. Run `herdr plugin unlink olafurns7.taskr`, then `taskr daemon --status`; use the reported PID to check `ps -p <pid> -o command=` shows `taskr daemon`, then stop that daemon with `kill <pid>`. Stop and report if the command does not match.
Remove only skill symlinks that resolve to the installed skill, then the installed taskr binary, `$HOME/.local/share/taskr/plugin`, and the installed skill directory. Use custom paths if configured. Preserve `$HOME/.local/state/taskr` and all reports.
Verify `herdr plugin list --json` lacks olafurns7.taskr and `test ! -e "${TASKR_INSTALL_DIR:-$HOME/.local/bin}/taskr"` succeeds. Expect the plugin and binary removed, with ledger history intact. Remove `taskr-tui` with `rm -f "${TASKR_INSTALL_DIR:-$HOME/.local/bin}/taskr-tui"` and verify with `test ! -e "${TASKR_INSTALL_DIR:-$HOME/.local/bin}/taskr-tui"`. Report removal and stop here; step 10 applies to installation.

## 10. Report to the user

Run `taskr version` and `taskr daemon --status` again; expect matching installed/running versions. Report:
```text
Installed: <version>; daemon: <running/stale/offline>; role: <local/hub/client>.
Terminal view: <taskr-tui installed / not installed>.
Skills linked: <paths or conflicts>; hooks: <yes/no, harness>.
Start: tell your orchestrator agent to load the taskr skill, then delegate work through Herdr and taskr.
```
