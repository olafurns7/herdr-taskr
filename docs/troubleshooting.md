# Troubleshooting

Start with these two commands; most answers are in their output.

```sh
taskr version
taskr daemon --status
```

The daemon's log is `~/.local/state/taskr/daemon.log`.

Agents have their own, more detailed guide:
[references/recovery.md](../references/recovery.md).

## taskr: command not found

The installer puts the binary in `~/.local/bin` (or `TASKR_INSTALL_DIR`). Add
it to your `PATH`:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Herdr's plugin looks in the same place; if you installed elsewhere, Herdr
needs `TASKR_INSTALL_DIR` in its environment too.

## The daemon is not running

`taskr daemon --status` shows `running: false`.

```sh
taskr daemon --restart
taskr daemon --status
```

The Herdr plugin normally starts the daemon when Herdr starts or detects an
agent. Check that the plugin is linked:

```sh
herdr plugin list --json | grep olafurns7.taskr
```

If it is missing, link it: `herdr plugin link "$HOME/.local/share/taskr/plugin"`.

## The daemon is an old version

`stale: true` in the status means the running daemon is older than the
installed binary, usually just after an upgrade. `taskr daemon --restart`
fixes it.

If `--restart` exits 6 and says a process was "not signalled", it refuses to
stop a process it cannot verify. Take the `pid` from `--status`, check that
it is a taskr daemon (`ps -p PID -o command=`), stop it with `kill PID`, and
the plugin starts a new one within seconds.

## taskr-tui says it needs a terminal

It draws a full-screen view, so its input and output must be a terminal. In a
script or a pipe, use `taskr glance` for the same data.

## taskr-tui shows no data or a stale frame

`taskr-tui` runs `taskr --json glance` every few seconds. Run that command
yourself to see the error.

- `taskr` not found: fix `PATH`, or point `TASKR_BIN` at the binary.
- "Cannot reach the hub": on a client host, see the next section.
- "No open campaigns" is not an error: the ledger has nothing open. Try
  `taskr-tui --demo` to see what a busy one looks like.

## The hub is unreachable from a client

A command exits 5 with `server unreachable`. Check, on the client:

```sh
cat ~/.local/state/taskr/server.url   # the hub's URL, e.g. http://hub.example.ts.net:7788
tailscale status                      # is this machine on the tailnet?
```

And on the hub: `taskr daemon --status` should show `role: hub` and a
`tailnet_url`. If `tailnet_url` is missing, Tailscale was not up when the
daemon tried; it retries on its own, and the log says why.

A 403 means the hub refused this machine: both must belong to the same
Tailscale user, and neither may be a tagged node.

There is no local fallback. Commands do not quietly write to a ledger on the
client.

## A command printed qd1

`qd1 <key>` with exit 0 means "queued": the hub could not be reached, and the
record waits in `~/.local/state/taskr/spool/`. Do not run the command again.
The client's daemon sends the queue when the hub returns.

```sh
taskr spool ls     # queued, refused and bad records
taskr spool send   # try to send now
```

A record that stays refused carries the hub's reason. Read it before removing
the record with `taskr spool rm`.

## A command exits 6

Exit 6 is a refusal, not a failure to retry: a closed task, an agent that was
relaunched and is using its old launch ID, or a command run on the wrong
host. The message says which. Retrying the same command will be refused again.

## No notification for an owner ask

Sound notifications fire only for blocking owner asks. Every owner ask still
shows in `taskr-tui` under "Needs you" and in `taskr asks --owner --open`.

Notifications come from the daemon through Herdr. If `taskr daemon --status`
shows `daemon: stale` or `none`, the daemon has no Herdr connection; if it
shows `herdr_missing: true`, the daemon cannot find the `herdr` program on its
`PATH` (common under systemd; see
[install.md](install.md#start-the-ledger-host-at-boot-linux)).

## Port 7788 is taken

The log says `listen 127.0.0.1:7788 failed`. The daemon keeps working, and
local commands never use this port. On a hub the Tailscale listener is bound
separately and usually still serves: check `tailnet_url` in
`taskr daemon --status`. Change the port only when `tailnet_url` is missing
too: write `tailnet:PORT` to `~/.local/state/taskr/dashboard.addr`, run
`taskr daemon --restart`, and put the new port in each client's `server.url`.

## A host still uses hub.url

`role: peer` in the status. That setup pushed a copy of the host's ledger to a
web page on the hub, which no longer exists; nothing is pushed now. Remove
`hub.url`, or make the host a client:
[install.md, step 7](install.md#7-multiple-machines).

## Searching says the index is damaged

Stop the hub's daemon, drop the table `search_fts` in the ledger file, and
start the daemon again; it rebuilds the index.
