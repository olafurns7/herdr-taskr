# CLI format1

Compact default. --json (before command or command flag) / TASKR_FORMAT=json: legacy objects/JSONL, legacy fields/exits. Handover/adopt: Markdown; help: human text, exit0 also under --json. Compact errors once; JSON keeps legacy diagnostics. IDs absolute, from the ledger in use: the server's once a host is a client (server.url), else the host's own. Every compact line tagged/versioned.

Space-separated results:
```text
n1 TASK [planned]     # new
l1 {launch_id,...}    # replacement/planned fields: replaced_launch_id,status,was_planned
p1 {p,o,oe,g,gok,r,due} # prompt; optional fields absent if not returned
g1 EVENT ROUND [dup]
s1 EVENT [dup]        # start
n1 EVENT [dup]        # note; command disambiguates new
r1 EVENT [dup]        # ready
q1 ASK [dup]
d1 EVENT [dup]
f1 EVENT [dup]
k1 ACKED [already]    # ack, current cursor if already
c1 TASK [already]     # close
nx1 EVENT [clear]
qd1 REQUEST_KEY       # record queued for delivery
ds1 DOC VERSION [same] # doc set
dr1 DOC VERSIONS_REMOVED # doc rm --purge
rf1 [CHANGED_IDS]     # set; []=no changes
dc1 {e,k,dc}          # decide; k=revoke includes revoked=EVENT
a1 {a,sent,w,...}     # answer; transported: prompt fields too
sk1 COUNT            # skipped nonmatches (>0), before result
w1 {"owed":N,"due":K} # wait timeout counts, before x1 3 timeout
x1 3 timeout [interrupted|unreachable]
x1 EXIT JSON         # error + all partial-write/transport fields
j1 JSON              # reads, version, daemon
```
Compact wait timeout prints `w1` then `x1 3 timeout` and exits0; the frame's 3 is not the process exit code. Order: sk1, w1, x1. `owed`: open children whose newest prompt has no later done/fail; `due`: armed receipt deadlines addressed to the waiter. Interruption prints `x1 3 timeout interrupted` without `w1` and exits3. Legacy JSON wait timeout exits3 with `{"timeout":true,"as":ID,"owed":N,"due":K}`; interruption has `interrupted:true` and no counts.

Exits: 0 success or compact wait timeout; 2 usage; 3 interruption or legacy JSON wait timeout; 4 DB; 5 transport (server unreachable, Herdr delivery, `--confirm` no_receipt); 6 rejection (stale launch, closed task, launch on a root task, host mismatch, wait `--as` other than TASKR_TASK, second answer, request key reused with other arguments).

Client records (`got`, `ready`, `done`, `fail`, `decide`, `next`, `note`, `close`) queue when the server gives no reply within 3 s (including proxy 5xx without an RPC reply body), or at once when the tailnet check fails or earlier records wait in this host's spool: `qd1 <request key>`, exit 0; with `--json`: `{"queued":true,"request_key":…}`. A full or unwritable spool: exit 5 with `retry with:`; rerun that line. A running client daemon sends them in order after a pass that reaches the server, or use `taskr spool send` on that host; without a daemon they stay queued until manual send. Do not retry queued records or count them as failures. `spool ls|send|rm SEQ|FILE` manage this host's queue; states and recovery: [recovery.md](recovery.md#spool). Other client commands retry transient transport failures: `wait` until its deadline, others for 60 s (or their longer RPC budget) with the same key. Stderr once: `taskr: server unreachable; retrying until <RFC3339 UTC>; if interrupted, retry with: <retry>`; a still-running request says `request still running` instead of `server unreachable`. `<retry>` is the same command and key as the exit-5 `retry with:` line. `wait` keeps `taskr: server unreachable; retrying until <RFC3339 UTC>` without a retry command. The hook queues its records in the spool; the client-host prompt relay and daemon keep their existing behaviour. For those other commands, transport exit 5 is a transport failure that could not be retried (5xx, non-JSON, too large, unverified), the end of the window, or an interrupted write. A wait deadline returns timeout if any attempt connected, else transport exit 5. A connected-then-lost wait prints `x1 3 timeout unreachable` (exit0), or JSON with `"unreachable":true` next to `"timeout":true` (exit3); ledger counts are unknown, so `w1` / JSON `owed` and `due` are omitted.

Braces above abbreviate actual JSON objects with quoted keys. Result aliases: p=attempt_id,o=outcome,oe=outcome_event_id,g=receipt_event_id,gok=receipt,r=round,e=event_id,dc=decision_id,k=kind,a=answer_id,sent=delivered,w=asker_waiting,err=error,due=receipt_due_ms (armed receipt window; only on activity_observed). Unlisted details unchanged, including delivery_outcome_event_id. Success omits echoed inputs/ok; duplicate/replacement/clear state preserved. Errors retain returned task/target/ask ids. `--confirm` no_receipt: exit5,gok=false; answer may already be committed. Never re-answer/blindly resend. A prompt without `--confirm` reports a missed receipt later, as a `po` event.

Wait: nine literal TAB fields, one physical line:
```text
e1<TAB>EVENT<TAB>TASK<TAB>LAUNCH|-<TAB>KIND<TAB>REPLAY0|1<TAB>RELATED|-<TAB>JSON_STRING_SUMMARY<TAB>JSON_OBJECT_DATA
```
Kinds: g/r/q/a/oa/d/f/h=got/ready/ask/answer/owner_answer/done/fail/herdr; n/s/p/po/dc/rv/rf/nx/ho/ad/l/c=note/start/prompt/prompt_outcome/decision/revoke/ref/next/handover/adopt/launch/closed; do=doc (no wait recipient). Unknown names pass through. --for takes long names. `po` summaries: activity_observed, rejected, delivery_unknown, no_receipt (async: data `outcome`,`window_ms`,`async:true`; --confirm: `confirm_timeout_ms`), late_receipt (data `outcome`,`got_event_id`,`no_receipt_event_id`,`delay_ms`). Got generated summary empty, data.identity omitted; full log retains identity, frame retains round/related attempt. Missing data={}. Strings JSON escaped, never truncated; tabs/newlines/Unicode safe. data/kv/refs opaque; false != unknown.

Read j1 envelopes: i=id,t=task_id,n=task_name|name,k=kind,at=created_at|updated_at,to=recipient_task_id,l=launch_id|current_launch_id,rel=related_event_id,ans=answered_by,s=summary|status,d=data,key=event_key,rec=record,w=workspace_id,tab=tab_id,pane=pane_id,rp=report_path,par=parent_id,ack=acked_event_id,pend=pending_event_id,got=last_receipt,a=open_asks,b=blocking_asks,r=round,wait=waiting,h=observed,nx=next. Inside status h only: s=agent_status,seq=state_change_seq; at/present unchanged. Absent stays absent; unlisted fields unchanged. Context disambiguates aliases. No recursion into payloads/native identity; full log keeps hashes/timestamps/history.

`notes`: log-shaped j1 event records (d.owner=true for owner notes), whole text; `m1 {"older":N,"all":"--limit 0"}` for omitted rows. --json prints legacy event JSONL with no trailer.

Filtered JSON wait may add `{"as":ID,"skipped":COUNT}` before the usual result. Unfiltered JSON keeps the old envelope. Matching events never auto-acked; pending replays. --ack requires pending handled event; standalone ack idempotent. Nonmatches acked/retained in log. Bypass (passes BOTH --for and --from, replays until explicit ack): `herdr` with `data.reason` `model_capacity` or `stall`, or with a `data.quota` field. A filtered wait also skips, in `sk1`: plain `herdr` hints (no reason, no quota) whose lane is closed or relaunched, or that have a newer plain hint or a later ready/done/fail/ask from that launch; a `no_receipt` whose `got` exists and was not pending; the `late_receipt` of such a skipped `no_receipt`. Unfiltered waits skip nothing. Include `herdr` in parent filters for unhooked lanes.

Stall (hooked lanes): `h`, summary `worker turn stalled`, key `stall:PROMPT`, data reason=stall, optional `last` (newest report code r/d/f/q) and `error` (turn error code, `unknown` when the harness gave none). Plain stalls are skipped for orchestrator and sub-orchestrator lanes; error stalls always emit. Quota (`--scan-quota`): `h`, summary `quota limit hit` or `quota N% left`, key `quota:LAUNCH:limit|low:N`, data quota=limit|low, percent, pane_id.

Capacity uses the existing `h` kind and JSON `herdr` envelope: summary `Codex capacity warning observed; inspect helper before retry`, key `capacity:LAUNCH:EPISODE`. Data allowlist: reason=model_capacity, provider=codex, model=registered launch model, model_source=launch, pane_id, agent_name, episode, source=detection, action=inspect_before_retry. No quota/percent or raw pane/prompt/native session/credential fields. Handling: [recovery.md](recovery.md).

Documents: `doc set ID goal|plan [--name NAME] --file PATH`, `doc ls ID [--tree] [--kind K] [--versions] [--limit N]`, `doc get DOC_ID`, `doc rm DOC_ID --purge`, `doc backfill [--tree ID] [--dry-run]`. Get prints exact stored bytes; ls prints j1 document records (doc_id,t,k,n,version,bytes,format,captured,reason,event_id,source_path,source_host,backfill,at), latest per task/kind/name by default. Ls defaults to 100 rows and 32 KiB; m1 gives older count and `--limit 0` to read all. Backfill prints j1 counts captured/too_large/binary/missing/client/unchanged and works from any host, capturing files on that host. Goal is root-only; plan may be named. Purge removes every version and unshared blobs; the write-ahead log and earlier backups can still hold the text. RPC get/ls/backfill are fresh; set/rm are stored. Briefs are on the caller's host (a non-planned, non-gate `new` refuses a missing file); prompt files must exist there; reports are read on the lane's host when it runs the command; goal/plan files are read where `doc set` runs, with no copy to the ledger host.

`close ID [--outcome accepted|reworked|rejected|abandoned]` keeps its `c1` result; `log` includes the optional `outcome` in closed-event data. Search: `search QUERY [--root ID] [--kind K] [--limit N] [--raw]`; hits are j1 rows (src/ref/root/task/kind/name/at/snippet), default 20, max 100. Examples: `taskr search "retry policy" --root 42`; `taskr search 'timeout OR outage' --kind decision --limit 10 --raw`.
