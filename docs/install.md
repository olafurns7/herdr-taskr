# Install taskr for Herdr

Follow these steps in order for installation; for uninstall-only requests, go directly to step 8. Stop on failure; report the step, command, exit code, and expected result. Do not continue with a partial install.
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
Expect Darwin or Linux, arm64/aarch64 or x86_64/amd64, and all tools found. Compare Herdr's version numerically against the fetched manifest's floor. If Herdr is missing, too old, or the floor cannot be fetched, stop and tell the user. Go and Node are unnecessary.

## 2. Install

Use curl without signing in. Do not pipe to sh: a partial download could execute before failure is reported.
```sh
curl -fsSL -o taskr-install.sh \
  https://github.com/olafurns7/herdr-taskr/releases/latest/download/install.sh &&
  test -s taskr-install.sh &&
  TASKR_LINK_SKILLS=1 sh taskr-install.sh &&
  PATH="$HOME/.local/bin:$PATH" taskr version
```
Alternatively, use GitHub CLI:
```sh
# Download the installer through GitHub CLI, then install and link agent skills.
gh api repos/olafurns7/herdr-taskr/contents/install.sh \
  -H 'Accept: application/vnd.github.raw+json' > taskr-install.sh &&
  test -s taskr-install.sh &&
  TASKR_LINK_SKILLS=1 sh taskr-install.sh &&
  PATH="$HOME/.local/bin:$PATH" taskr version
```
Expect checksum verification for the binary, skill archive, and plugin archive, then a release version. On checksum or download failure, stop; do not run the partial installer.

| Option | Effect |
| --- | --- |
| `TASKR_INSTALL_DIR` | Binary directory; default `$HOME/.local/bin`. Export it before installing; use that directory in PATH and Herdr's plugin environment. |
| `TASKR_VERSION` | Release tag; unset selects latest. Export a user-selected tag before running either snippet. |
| `TASKR_LINK_SKILLS=1` | Link the installed skill into existing agent config directories; preserve conflicts. |
| `TASKR_SKILL_DIR` | Skill directory; default `$HOME/.agents/skills/taskr`. |
| `TASKR_NO_SKILL=1` / `TASKR_NO_PLUGIN=1` | Skip the corresponding archive and setup. |

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
herdr plugin link "$HOME/.local/share/taskr/plugin"
taskr version
taskr daemon --status
```
Expect plugin registration and daemon fields `running: true`, the installed `running_version`, and a dashboard URL, normally http://127.0.0.1:7788/. A successful status exit alone does not prove the daemon is running.
If it is absent, tell the user to run `taskr daemon` in a separate terminal, then verify status again. For upgrades, have the owner run `taskr daemon --restart` outside the agent session. This restarts taskr, never Herdr.

## 5. Verify skills

The installer copies SKILL.md and references/ to `$HOME/.agents/skills/taskr` (or TASKR_SKILL_DIR). With TASKR_LINK_SKILLS=1 it links only existing `$HOME/.claude`, `$HOME/.codex`, and `$HOME/.config/opencode` directories:
```sh
skill_dir=${TASKR_SKILL_DIR:-$HOME/.agents/skills/taskr}
test -f "$skill_dir/SKILL.md" && test -d "$skill_dir/references"
for config in "$HOME/.claude" "$HOME/.codex" "$HOME/.config/opencode"; do
  if [ -d "$config" ]; then readlink "$config/skills/taskr" && test "$config/skills/taskr" -ef "$skill_dir" || exit 1; fi
done
```
Expect the installed skill and each applicable link to resolve to it. If configs do not exist, report no links; if a conflict exists, report it and leave it unchanged. Tell the orchestrator agent to load the installed taskr skill before coordinating workers.

## 6. Agent hooks

ASK the user before any hook or settings edit. Without approval, skip hooks and report them disabled. Neither the installer nor the Herdr plugin configures hooks.
After approval, merge command entries into existing Claude Code settings.json or Codex hooks.json using the installed harness's format; never replace the file. Use `claude` with SessionStart, UserPromptSubmit, Stop, StopFailure; `codex` with SessionStart, UserPromptSubmit, Stop. Template for each event:
```sh
[ -n "${TASKR_LAUNCH:-}" ] && taskr hook claude UserPromptSubmit >/dev/null 2>&1; true
```
For Claude Code, an entry is `{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"[ -n \"${TASKR_LAUNCH:-}\" ] && taskr hook claude UserPromptSubmit >/dev/null 2>&1; true","async":true}]}]}}`; add the other events with matching arguments.
OpenCode needs a custom plugin invoking `taskr hook opencode EVENT`, writing one JSON object to stdin, then closing it. Forward session.created/session.idle/session.error objects; for chat.message use `{"input": input, "output": output}`. No OpenCode hook plugin is shipped.
Verify `taskr hook --help` succeeds and inspect the merged entries against the event list. Expect valid harness config; runtime receipts require HERDR_ENV=1 and a registered TASKR_LAUNCH, configured before worker startup. Workers still run got and ready/done/fail explicitly. A receipt is acknowledgment, not completion; a stall is a signal to investigate.

## 7. Multiple machines

Only configure this if the user asks. Require running Tailscale with untagged nodes owned by the same user; identity checks reject tagged nodes and other users. HTTP traffic stays inside the encrypted tailnet.
On the hub, write `tailnet` to `$HOME/.local/state/taskr/dashboard.addr`. On a fresh client, write the hub URL (for example http://hub.example.ts.net:7788) to `server.url` in that directory; leave TASKR_DB unset. There is no local fallback when the hub is unreachable.
For independent ledgers and a combined read-only dashboard, use `hub.url` instead; do not combine it with server.url. Before migrating an existing ledger, back it up and read https://raw.githubusercontent.com/olafurns7/herdr-taskr/master/references/recovery.md.
Have the owner apply changes with `taskr daemon --restart`; verify `taskr daemon --status` shows the selected mode/listeners and fresh connection or push health. On write exit 5, rerun the exact printed `retry with:` command, preserving its request key.

## 8. Uninstall

Only uninstall if requested. Run `herdr plugin unlink olafurns7.taskr`, then `taskr daemon --status`; have the owner stop the reported daemon PID outside the agent session (or Ctrl-C its foreground terminal).
Remove only skill symlinks that resolve to the installed skill, then the installed taskr binary, `$HOME/.local/share/taskr/plugin`, and the installed skill directory. Use custom paths if configured. Preserve `$HOME/.local/state/taskr` and all reports.
Verify `herdr plugin list --json` lacks olafurns7.taskr and `test ! -e "${TASKR_INSTALL_DIR:-$HOME/.local/bin}/taskr"` succeeds. Expect the plugin and binary removed, with ledger history intact. Report removal and stop here; step 9 applies to installation.

## 9. Report to the user

Run `taskr version` and `taskr daemon --status` again; expect matching installed/running versions and the verified dashboard URL. Report:
```text
Installed: <version>; daemon: <running/stale/offline>; dashboard: <URL>.
Skills linked: <paths or conflicts>; hooks: <yes/no, harness>.
Start: tell your orchestrator agent to load the taskr skill, then delegate work through Herdr and taskr.
```
