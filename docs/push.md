# Push wake adapters

`taskr mcp --channel` pushes a task's inbox events into an idle or busy agent session instead of waiting for the agent to run `taskr wait`. Two clients can carry the push: a Claude Code channel, and pi through the `adapters/pi/taskr-push.ts` extension. This repository configures neither: nothing here edits an `.mcp.json`, a settings file, a Herdr launch flag or an install. This page says how an owner would opt in, what the push promises, and what is still unproved.

How the loop holds the inbox, what a push carries and how it reconciles uncertain writes: [mcp.md](mcp.md#channel).

## What it promises

- At least once, not exactly once. An event can reach the model more than once: up to 3 pushes per server process without a receipt, a restart starts a new budget, and a push already written can arrive after a handoff. The event id identifies the work; the model checks the inbox before handling and does not repeat handled work.
- A push is not an ack, and not a receipt. The server records `delivery new` and `delivery submitted`; only the model records `delivery received` and acks after handling. Submitted means the notification line was written and flushed, nothing more: a Claude session that is not registered as a channel drops it silently. That case is visible as submitted attempts with no receipt (`taskr delivery ls --open`) and the event still pending.
- A stall longer than the lease stops it. If the loop cannot renew within 60 s (a hub outage, a stuck command), it pushes nothing on the stale lease, the refused renew stops it for good, and the client must be restarted to resume pushes.
- One event at a time. The fenced wait offers the oldest pending event first, so a later event is not pushed while an earlier one is unacked. If the model never acks, pushes of that event stop after 3, and the inbox waits for an ack, a handoff or a restart.
- The lease fences consumption. While the server holds it, a plain `taskr wait` or `ack` on the inbox is refused (exit 6) and names the holder.

## Claude Code (research preview)

Register the server in the session's `.mcp.json` and start Claude with the development-channel flag:

```json
{"mcpServers": {"taskr": {"command": "taskr", "args": ["mcp", "--channel"]}}}
```

```sh
claude --dangerously-load-development-channels server:taskr
```

A worker session inherits `TASKR_TASK` and `TASKR_LAUNCH`; a root adds `"--as", "ROOT", "--session", "GEN"` to `args`, where GEN is the event id `taskr adopt` printed (0 before any adopt). Channels are a research preview: the flag shows a confirmation dialog, is ignored with `-p` and the SDK, needs claude.ai or Console API-key authentication (not Bedrock, Vertex or Foundry), and Team or Enterprise organizations must set `channelsEnabled`. Events that arrive while Claude is busy queue and are delivered together on the next turn. The model sees `<channel source="taskr" event="E" attempt="A" fence="N" kind="K" task="S">TEXT</channel>` and records the receipt and ack with the server's `taskr_received` and `taskr_ack` tools.

## pi

Load the extension on the command line:

```sh
pi --extension /path/to/herdr-taskr/adapters/pi/taskr-push.ts
```

Options: `--taskr-bin PATH` (default `taskr` on `PATH`), and for a root `--taskr-root ID --taskr-session GEN` (both, as integers). A worker needs nothing: the extension starts `taskr mcp --channel` with pi's own environment. Each push becomes `pi.sendUserMessage(content, {deliverAs: "followUp"})`, which starts a turn when idle and queues after the current one when busy. The extension adds no tool: a pi model records the receipt and ack with the CLI lines in the push (`taskr delivery received A`, `taskr ack E --as SELF --fence N`, a root adding `--session GEN`), or through a separately configured plain `taskr mcp` with the same binding. On shutdown the extension stops forwarding for good (a late handshake reply enables nothing), closes the child's stdin (the lease is released) and kills its process group after 5 s; whenever the child exits, anything left in its process group is killed, and shutdown completes once the group is empty. A child that exits on its own is logged and not restarted.

`node adapters/pi/check.mjs [TASKR]` checks the extension with a mocked pi API, a real `taskr mcp --channel` on a scratch ledger and a fake child. It proves the translation and the cleanup, not pi's loader, its queue or a provider turn.

## Handing the inbox back to `taskr wait`

1. Stop the client (Claude or pi). Stdin EOF makes the server stop pushing, finish its current step and release the lease; a killed server's lease expires after 60 s.
2. If the server cannot be stopped, `taskr consumer release --fence N` from the same identity, or `taskr consumer take --force` to take the inbox over: the old server stops at its next renew. A push it already had in flight can still reach that session.
3. Run `taskr wait`. A pending event is offered again from the same cursor.

## Evidence and what is still owed

| Stack gate | Proof |
| --- | --- |
| idle and busy delivery, model-issued receipt and ack, per client | real-client trial: owed (a separate, bounded trial lane; not run in this change) |
| dropped notification, broken stdout | `a_broken_stdout_leaves_the_attempt_unsubmitted_and_ends_delivery` |
| ignored notification (the T1 failure mode), retry exhaustion | `redelivery_is_bounded_and_receipts_suppress_it_across_restarts` |
| reset and reconnect | `hints_wake_a_check_and_a_reset_reconnects_without_a_duplicate` (the hub's ten-minute stream expiry ends the stream the same way) |
| duplicate and late receipts, RPC uncertainty | `lost_replies_reconcile_with_their_own_keys_and_a_receipt_may_precede_submitted`, `concurrent_same_binding_servers_never_share_the_lease`, `a_takeover_between_attempt_and_write_allows_one_late_push_then_stops`, `a_failed_show_after_a_replayed_take_keeps_its_key_and_never_retakes`, `an_uncertain_local_new_never_claims_an_older_attempt` |
| lease lifecycle, takeover, relaunch, adopt, explicit wait handoff | `the_lease_is_taken_renewed_and_released_with_its_fence_kept`, `a_refused_take_is_retried_and_never_forced`, `a_stale_binding_take_is_final`, `a_takeover_stops_pushes_and_the_tools_keep_serving`, `an_adopt_stops_a_root_channel_whose_envelope_uses_the_pinned_session`, `a_relaunch_stops_the_old_channel_and_an_old_receipt_does_not_suppress_the_new_one`, `a_killed_server_expires_and_a_plain_wait_consumes_the_same_event` |
| timeout and cancel, stalls | unchanged from `taskr mcp`: a started tool command runs to completion; the loop's commands are separate children. `a_stalled_child_keeps_the_tools_serving_and_stale_authority_never_pushes`, `eof_during_a_channel_child_pushes_nothing_then_releases`, `eof_during_a_tool_stops_new_pushes_and_the_tool_still_finishes`, `stdin_read_ahead_is_bounded_while_a_tool_is_held`, `eof_behind_a_full_queue_never_pushes_and_releases`, `a_backlog_pause_keeps_an_unresolved_new_and_its_key`; the client-host cases run with a stalled hint stream |
| envelope and hostile summaries | `hostile_summaries_stay_one_escaped_untrusted_line`, `an_idle_event_is_pushed_once_then_received_acked_and_the_next_follows` |

These live in `crates/taskr/tests/channel.rs` and run with the `contract` feature, whose test-only scheduler divides every interval; release builds have fixed intervals (60 s lease, 20 s renew, 30 s poll and retake, 120 s spacing, 3 pushes) and no knob. Still owed before rollout: the real-client trial above, macOS, and any enablement.
