# taskr mcp

`taskr mcp` is a local stdio [MCP](https://modelcontextprotocol.io) server, revision 2025-06-18 only: newline-delimited JSON-RPC 2.0 on stdin and stdout (stdout carries only protocol lines; diagnostics go to stderr). It does nothing until an agent is configured to start it; this repository configures none.

Identity is fixed at start and never taken from a call:

| Start | Bound to |
| --- | --- |
| `taskr mcp` with `TASKR_TASK` and `TASKR_LAUNCH` | the worker's task and that launch |
| `taskr mcp --as ROOT --session GEN` without `TASKR_LAUNCH` | the root and generation GEN (the event id `taskr adopt` printed, 0 before any adopt), pinned |

Anything else exits 2 before serving: no identity, `--session` for a worker, `--as` naming another task, `--request-key`. Every input schema has `additionalProperties: false`, so an `as`, `task`, `session`, `recipient` or `request_key` argument is invalid params (-32602) and nothing runs.

Each call runs this same build (`/proc/self/exe` on Linux) as `taskr --json CMD ...` with stdin closed, the server's start directory and environment, the server-owned flags first and any text after `--`. Calls are serial; a started command always finishes, also after `notifications/cancelled` or stdin EOF, and EOF then exits 0.

## Resources and tools

| Name | Runs | Mode |
| --- | --- | --- |
| `taskr://self/task`, tool `taskr_task` | `status --tree SELF` (the task, its descendants and the daemon record, as an array) and `consumer show`: `{status:[...], consumer:{...}}`; either failing fails the whole read | both |
| `taskr://self/inbox`, tool `taskr_inbox {limit?}` | `inbox`: raw, non-consuming (see [cli.md](cli.md)) | both |
| `taskr_received {attempt}` | `delivery received ATTEMPT` (a root adds `--as ROOT --session GEN`) | both |
| `taskr_ack {event, fence}` | `ack EVENT --as SELF --fence N` (a root adds `--session GEN`); `fence` is required: a plain ack does not check the launch | both |
| `taskr_note {text}`, `taskr_ask {text, blocking?}` | `note -- TEXT`, `ask [--blocking] -- TEXT` (never an owner ask) | worker |
| `taskr_ready {summary, report_path?}` | `ready [--report ABS_PATH] -- SUMMARY`; a relative path is invalid params | worker |
| `taskr_done {summary}`, `taskr_fail {summary}` | `done -- SUMMARY`, `fail -- SUMMARY` | worker |

Reading never offers, receipts or acks. A receipt is not an ack. No tool answers, prompts, creates, launches, closes, adopts, sets, decides, waits, takes a lease or records a delivery attempt.

## Results

A resource read returns `contents:[{uri, mimeType:"application/json", text}]`; a failed read is a JSON-RPC error (-32603) whose `data` holds the outcome. A tool result carries the command's stdout as text and `structuredContent` with its JSON fields plus `exit`, `outcome`, `stderr` when any, and on a client host `retry_handle`:

| outcome | Meaning | isError |
| --- | --- | --- |
| `committed` | exit 0: the ledger command committed (or a same-key replay of its stored result). A ready report's capture is best effort, as in the CLI. | no |
| `queued` | exit 0 with `{"queued":true,"request_key":K}`: in this host's spool, not committed. Never retry it. | no |
| `unknown` | exit 5 saying `outcome unknown`, or a command that started but gave no whole JSON answer: it may have committed | yes |
| `transport` | any other exit 5: the execution state is unconfirmed, not failed | yes |
| `rejected`, `usage`, `error` | exit 6, exit 2, anything else | yes |

The server never retries. After `unknown` or `transport`, inspect and reconcile (`taskr log`, `consumer show`, `delivery ls`) before retrying; to retry, repeat the identical call with its `retry_handle`. On a client host (a `server.url` without `TASKR_DB`) each write gets a fresh handle; the request key it maps to is derived from the bound task, launch or generation and the handle, so two bindings never share a key, and a handle names one call (another call with it is invalid params). A replayed result is historical: it never shows that a launch, generation or lease is still current. On a ledger host there are no handles.

Framing: an empty line is ignored; invalid JSON, invalid UTF-8 and lines over 1 MiB get -32700 with id null; a batch, a non-object or a bad envelope gets -32600 (id null unless a valid string or integer id can be read); an unknown method -32601; bad params -32602. Lifecycle: `initialize` once (a second is -32600), then `notifications/initialized`; until then every request but `ping` is -32600. Notifications never get a reply, and unknown ones run nothing.
