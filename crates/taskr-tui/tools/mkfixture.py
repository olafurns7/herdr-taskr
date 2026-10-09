#!/usr/bin/env python3
"""Write the synthetic fixture the frames are drawn from. Usage: mkfixture.py [--live] [OUT]

Nothing here comes from a ledger: campaign names, ask texts, task ids, hosts and PR
numbers are invented. The shapes follow `taskr --json glance` plus the fields P1a and
P1b add, and the lengths and edge cases follow real use: a 400-character ask from another
machine, a campaign with 29 lanes and a sub-orchestrator, a stale host, a parked
campaign, a lead that is idle with results, a note that says only "OWNER: nothing.".
Text stays ASCII so every glyph in a frame is one the view drew. `--live` writes
fixture-live.json instead: the same data with the text live ledgers hold too, emoji with
VS16, a ZWJ sequence and CJK, for the tests that wide glyphs must not crash or garble.
"""
import json, os, random, sys

H, M, S = 3600000, 60000, 1000
LAPTOP, MINI = "sam-laptop", "build-mini"
rng = random.Random(7)
at = lambda hhmmss: "2026-03-14T%s.000Z" % hhmmss


def spark(shape):
    """24 ten-minute buckets, oldest first."""
    if shape == "burst":
        return [0] * 22 + [rng.randint(20, 40), rng.randint(8, 20)]
    if shape == "late":
        return [0] * 20 + [rng.randint(3, 9), rng.randint(5, 12), 0, rng.randint(12, 24)]
    if shape == "early":
        return [rng.randint(0, 14) for _ in range(12)] + [rng.choice([0, 0, 0, 1, 2]) for _ in range(12)]
    return [rng.choice([0, 0, 1, 3, 6, 9, 14, 22]) for _ in range(24)]


def last(event_id, kind, age, text):
    return {"event_id": event_id, "kind": kind, "age_ms": age, "text": text}


def camp(id, name, lead, waiting, lanes, age, shape, text, kind="ready", host="", note=None, **extra):
    row = {"id": id, "name": name, "host": host, "pane_id": "w%02d:p1" % (id % 97), "lead": lead, "lead_waiting": waiting,
           "lanes": dict(zip(("working", "ready", "open"), lanes)), "activity_age_ms": age, "spark": spark(shape),
           "last": last(60000 + id, kind, age, text)}
    if note:
        row["owner_note"] = last(60500 + id, "note", age, note)
    row.update(extra)
    return row


SHORT_ASK = ("Frames follow-up is ready: PR #212 (orphaned asks hidden, close warns). (A) merge #212 and release v2.4.1 now "
             "[recommended]; (B) merge, hold the release for the next change.")
LONG_ASK = ("WIDGET BATCH: 19 of 23 merged (14:02-14:09Z, squash). STOPPED, per your rule (conflict on the way): #641 and the "
            "three stacked children #638, #655, #652. A worker merged main into each branch, resolved the conflicts and "
            "pushed. Hosted CI is green on all four heads.\n"
            "Question: (A) merge these four now (#641, #638, #655, #652), then one beta build from main [recommended]; "
            "(B) build now with the 19; these four wait for a later build.\n"
            "If there is no answer in 15 minutes, I take (B).")

campaigns = [
    camp(4310, "search-index", "working", False, (1, 0, 1), 19 * S, "burst", "Research complete: reports/ranking-options.md compares three scorers on the saved queries", "done"),
    camp(4288, "auth-rotation", "working", False, (0, 1, 1), 29 * S, "late", "Implementation slice: key rollover job, fake-clock tests, installer and operator docs"),
    camp(4171, "billing-export", "idle", True, (1, 0, 1), 33 * S, "busy", "FYI 61448 from search-index (root 4310): the tokenizer upgrade drops the legacy stemmer", "note"),
    camp(4296, "docs-refresh", "idle", True, (1, 0, 1), 2 * M, "burst", "Hub FYI: keep the page map machine-readable for the future link checker", "decision"),
    camp(4052, "checkout-flow", "idle", True, (1, 1, 2), 3 * M, "busy", "FIX: three P2 findings in the review of the address step; source review complete", "done"),
    camp(4087, "ios-widgets", "working", False, (0, 0, 0), 7 * M, "early", "nothing open", "note", host=LAPTOP,
         note="Widget batch approved (asks 61199, 61367): 19 PRs merged; the last 4 wait for your answer."),
    camp(4120, "tui-frames", "working", False, (3, 1, 6), 8 * M, "busy", "Second review done: three wording fixes, then it can go", "done",
         note="OWNER: nothing. NOW: plan rev 3.1 sent; the trust rules and the first frames are in flight."),
    camp(4031, "infra-layout", "idle", False, (0, 0, 0), 12 * M, "busy", "cache-audit lead root=4040 pane=w41:p1: campaign complete, docs PR merged", "note"),
    camp(4215, "model-evals", "working", False, (5, 2, 9), 14 * M, "early", "matrix run A finished"),
    camp(4066, "api-pagination", "idle", True, (1, 0, 1), 54 * M, "early", "S4 cursor split merged; contract tests green on both hosts", "done", note="OWNER: nothing."),
    camp(3254, "legacy-importer", "idle", False, (0, 0, 0), 3 * H + 9 * M, "idle", "Held by the owner until the import budget is decided.", "note",
         parked=True, park_age_ms=3 * H),
]
campaigns[-1]["spark"] = [0] * 24

# A runbook ask longer than the glance's detail pane, for the frame that scrolls it. Not in
# the glance: frames.rs adds it where it is drawn, so the other frames stay as they are.
STEPS_ASK = ("Release runbook for v2.5, ready to run once you pick the window. Each step waits for the one before:\n"
             + "\n".join(f"{i}. {s}" for i, s in enumerate([
                 "Freeze the release branch and post the freeze note in the team channel.",
                 "Tag the release candidate from the head of the release branch.",
                 "Build the server, the client and the daemon for both architectures.",
                 "Run the full suite on the build host with the slow tests switched on.",
                 "Install the candidate on the staging host and replay one day of traffic.",
                 "Compare the replay's error rate and latency with the last release.",
                 "Write the changelog from the merged pull requests since v2.4.",
                 "Publish the candidate to the beta channel and wait one hour for crash reports.",
                 "Promote the candidate to stable if the beta channel stays quiet.",
                 "Update the install docs and the upgrade notes for the new flags.",
                 "Unfreeze the release branch and merge it back into main.",
                 "Close the release milestone and archive the runbook with the timings.",
             ], 1))
             + "\nWhen: (A) tonight after 22:00 [recommended]; (B) tomorrow morning.")
long_ask = {"kind": "owner_ask", "campaign": "infra-layout", "root_id": 4031, "host": "", "pane_id": "w62:p1",
            "age_ms": 3 * M + 12 * S, "since": at("14:12:58"), "ask_id": 61812, "blocking": True,
            "asker": "infra-layout", "asker_task_id": 4031, "asker_waiting": True, "text": STEPS_ASK}



def structured(campaign, root_id, pane, age, ask_id, blocking, question, context="", dialog=False):
    # As `taskr ask --question` stores it (Q1): the summary is `[context ][header: ]question
    # (A) label; (B) label`, and `question` and `dialog` ride along.
    labels = "; ".join("(%s) %s" % (chr(65 + i), o["label"]) for i, o in enumerate(question["options"]))
    text = (context + " " if context else "") + question["header"] + ": " + question["question"] + " " + labels
    ask = {"kind": "owner_ask", "campaign": campaign, "root_id": root_id, "host": "", "pane_id": pane, "age_ms": age,
           "since": at("14:16:10"), "ask_id": ask_id, "blocking": blocking, "asker": campaign, "asker_task_id": root_id,
           "asker_waiting": True, "text": text, "question": question}
    if dialog:
        ask["dialog"] = True
    return ask


# Kept out of the glance, as the long ask is: the frames and `--demo` put them in front.
structured_asks = [
    structured("fleet-hub", 3001, "w1:p1", 25 * S, 61811, True, dialog=True, question={
        "header": "Release", "question": "Cut the v3.2 release today, or wait for the cache fix?", "multiSelect": False,
        "options": [
            {"label": "Cut today", "description": "Ships the 14 merged changes now; the cache fix follows in v3.2.1."},
            {"label": "Wait for the fix", "description": "One release with everything, likely tomorrow afternoon."},
        ]}),
    structured("auth-rotation", 4288, "w20:p1", 2 * M + 5 * S, 61790, True, question={
        "header": "Tokens", "question": "Which signing scheme should the rotated service tokens use?", "multiSelect": False,
        "options": [
            {"label": "Ed25519", "recommended": True,
             "description": "Short keys and fast checks; every client library we ship supports it already."},
            {"label": "RSA-2048", "description": "Matches today's tokens, so no client changes, but rotation takes four times as long."},
            {"label": "HMAC-SHA256", "description": "Simplest to run, but every verifier then holds the signing secret."},
        ]}),
    structured("docs-refresh", 4296, "w28:p1", 4 * M + 40 * S, 61802, False,
               context="Translations landed for three of the four locales.", question={
        "header": "Locales", "question": "Which locales ship in the first docs release?", "multiSelect": True,
        "options": [
            {"label": "English", "description": "The source text; always complete."},
            {"label": "German", "description": "Reviewed by the partner team last week."},
            {"label": "Japanese", "description": "Machine draft; 40 pages are still unreviewed."},
            {"label": "Portuguese", "description": "Half translated; the glossary is not settled yet."},
        ]}),
]

glance = {
    "now": at("14:16:10"), "verdict": "needs_you", "owner_notes_pending": 3, "server_host": "atlas", "caller_host": "",
    "needs_you": [
        {"kind": "owner_ask", "campaign": "tui-frames", "root_id": 4120, "host": "", "pane_id": "w31:p1", "age_ms": 41 * S,
         "since": at("14:15:29"), "ask_id": 61767, "blocking": True, "asker": "tui-frames", "asker_task_id": 4120,
         "asker_waiting": True, "text": SHORT_ASK},
        {"kind": "owner_ask", "campaign": "ios-widgets", "root_id": 4087, "host": LAPTOP, "pane_id": "w95:p1", "age_ms": 6 * M + 58 * S,
         "since": at("14:09:11"), "ask_id": 61367, "blocking": False, "asker": "ios-widgets", "asker_task_id": 4087,
         "asker_waiting": True, "text": LONG_ASK},
    ],
    "attention": [
        {"kind": "lead_unregistered_silent", "campaign": "map-tiles-polish", "root_id": 4233, "host": LAPTOP, "pane_id": "",
         "age_ms": 2 * H + 4 * M, "since": at("12:12:10"), "text": "lead unregistered and silent"},
        {"kind": "lead_idle_results", "campaign": "infra-layout", "root_id": 4031, "host": "", "pane_id": "w54:p1",
         "age_ms": 2 * H + 10 * M, "since": at("12:06:10"), "count": 3, "text": "lead idle with 3 results"},
        {"kind": "host_stale", "campaign": "host build-mini", "root_id": 0, "host": MINI, "pane_id": "", "age_ms": 12 * M,
         "since": at("14:04:10"), "text": "host stale: no heartbeat for 12 m"},
    ],
    "campaigns": campaigns,
    "quiet": {"count": 2, "names": ["cache-audit", "font-swap"], "root_ids": [3871, 3990]},
}

ROOT = 4120


def lane(id, name, role, model, effort, age, *, status="closed", state="", outcome="accepted", parent=ROOT, depth=1, host="", pane=None, summary=None):
    provider = "claude" if model.startswith("claude") else "codex"
    if summary is None:
        summary = "" if status == "open" else "%s: done and validated; report captured" % name
    return {"id": id, "parent_id": parent, "depth": depth, "name": name, "role": role, "status": status, "state": state,
            "outcome": outcome if status == "closed" else "", "host": host, "provider": provider, "model": model, "effort": effort,
            "pane_id": "w%02d:p%d" % (id % 89, id % 5 + 1) if pane is None else pane, "summary": summary, "age_ms": age,
            "brief": True, "report": bool(summary)}


OPUS, FABLE, SOL, ASTRA = "claude-opus-5-5", "claude-fable-5-1", "gpt-6.1-sol", "gpt-6-astra"
lanes = [
    lane(4190, "sub-list-core", "sub-orchestrator", OPUS, "high", 4 * M, status="open", state="working", host=LAPTOP),
    lane(4192, "rev-list-harness", "reviewer", OPUS, "medium", 21 * M, status="open", state="idle", host=LAPTOP, parent=4190, depth=2, pane=""),
    lane(4191, "impl-list-harness", "implementer", SOL, "high", 4 * M, status="open", state="working", host=LAPTOP, parent=4190, depth=2),
    lane(4185, "impl-frames", "implementer", OPUS, "medium", 40 * S, status="open", state="working"),
    lane(4183, "impl-trust-rules", "implementer", SOL, "high", 2 * M, status="open", state="working"),
    lane(4176, "arch-plan-counter2", "researcher", FABLE, "xhigh", 9 * M, status="open", state="ready",
         summary="Second review done: hold the send until three wording fixes land (P1-1 short rows, P1-2 counts, P1-3 install path)."),
]
closed = [
    ("rs-visual-spec", "researcher", OPUS, "high", 11, "accepted"), ("rs-visual-spec", "researcher", OPUS, "high", 10, "abandoned"),
    ("arch-plan-counter", "researcher", FABLE, "xhigh", 10, "accepted"), ("arch-rust-path", "researcher", ASTRA, "high", 33, "accepted"),
    ("rs-interaction", "researcher", OPUS, "high", 45, "accepted"), ("arch-stack-choice", "researcher", ASTRA, "high", 42, "accepted"),
    ("arch-trust-rules", "researcher", ASTRA, "high", 45, "accepted"), ("rev-hidden-asks", "reviewer", OPUS, "medium", 125, "accepted"),
    ("impl-hidden-asks", "implementer", SOL, "high", 130, "accepted"), ("rev-list-polish", "reviewer", OPUS, "medium", 140, "accepted"),
    ("impl-list-polish", "implementer", SOL, "high", 185, "reworked"), ("arch-first-counter", "researcher", FABLE, "xhigh", 190, "accepted"),
    ("impl-watch-fix", "implementer", SOL, "high", 200, "accepted"), ("rev-watch-mode", "reviewer", OPUS, "medium", 205, "accepted"),
    ("impl-lead-state", "implementer", SOL, "high", 250, "accepted"), ("impl-snapshot-fix", "implementer", SOL, "high", 260, "accepted"),
    ("impl-watch-mode", "implementer", SOL, "high", 265, "accepted"), ("rev-snapshot", "reviewer", OPUS, "medium", 270, "rejected"),
    ("impl-lead-fix", "implementer", SOL, "high", 300, "accepted"), ("rev-lead-liveness", "reviewer", ASTRA, "high", 290, "accepted"),
    ("impl-lead-liveness", "implementer", OPUS, "high", 310, "accepted"), ("impl-snapshot", "implementer", SOL, "high", 320, "accepted"),
    ("arch-first-critique", "researcher", ASTRA, "medium", 350, "accepted"),
]
for i, (name, role, model, effort, minutes, outcome) in enumerate(closed):
    lanes.append(lane(4170 - 2 * i, name, role, model, effort, minutes * M, outcome=outcome,
                      summary="" if outcome == "abandoned" else None))


def ev(id, kind, hhmmss, lane, text, **more):
    return dict({"id": id, "kind": kind, "at": at(hhmmss), "lane": lane, "text": text}, **more)


D4 = "D4: one binary for the view and the hub; the old suite stays the oracle until parity."
NOTE = "OWNER: nothing. NOW: plan rev 3.1 sent; the trust rules and the first frames are in flight."
campaign = {
    "root": {"id": ROOT, "name": "tui-frames", "status": "open", "created_at": at("08:11:24"), "host": "", "pane_id": "w31:p1",
             "lead": "working", "age_ms": 14 * S,
             "next": "trust rules (lane 4183) and the first frames (lane 4185) -> owner yes on the look -> key handling"},
    "goal": ["tui-frames: a small terminal view of every campaign and open question",
             "A split pane next to the hub that shows key status updates per campaign and anything urgent that needs the owner."],
    "plan": {"version": 11, "decisions_since": 1, "closed_since": 2},
    "lanes": lanes,
    "asks": [
        # The same open ask the glance shows for this campaign: the two views must agree.
        ev(61767, "ask", "14:15:29", "tui-frames", SHORT_ASK, open=True, owner=True, blocking=True),
        ev(61192, "answer", "13:31:14", "arch-rust-path", "Don't wait for the stack report. Finish the transport and the web inventory on your own evidence."),
        ev(61188, "ask", "13:31:02", "arch-rust-path", "The stack report is not present yet. Please provide it or say to proceed without."),
        ev(61152, "answer", "13:24:19", "arch-trust-rules", "The hub did not say which two it counted, and I won't guess for you. List all five."),
        ev(61151, "ask", "13:24:00", "arch-trust-rules", "Evidence conflicts with the brief's two real owner actions: I count five. Which two?"),
    ],
    "decisions": [ev(61301, "decision", "13:58:40", "tui-frames", D4),
                  ev(60790, "decision", "11:55:12", "tui-frames", "Trust rule 1: red is an open owner ask and nothing else.")],
    "docs": [{"id": 9560, "kind": "report", "name": "", "lane": "arch-plan-counter2", "version": 1, "captured": True},
             {"id": 9546, "kind": "plan", "name": "", "lane": "tui-frames", "version": 11, "captured": True}]
            + [{"id": 9500 - 9 * i, "kind": "report", "name": "", "lane": name, "version": 1 + (i % 4 == 3), "captured": i != 0}
               for i, name in enumerate(["rs-visual-spec", "arch-plan-counter", "arch-rust-path", "arch-stack-choice", "rs-interaction",
                                         "arch-trust-rules", "rev-hidden-asks", "impl-hidden-asks", "rev-list-polish", "impl-list-polish",
                                         "arch-first-counter"])]
            + [{"id": 9071, "kind": "goal", "name": "", "lane": "tui-frames", "version": 2, "captured": True}],
    # One row per PR-valued ref: a bare `pr` with its stored title, state, CI and review,
    # and two `pr.<slice>` rows from a lane that opened one per slice.
    "prs": [
        {"task_id": 4183, "lane": "impl-trust-rules", "key": "pr", "value": "215", "number": 215,
         "title": "feat(glance): trust rules, red means an open owner ask", "state": "open", "ci": "running", "review": "pending"},
        {"task_id": 4131, "lane": "impl-list-polish", "key": "pr.backend", "value": "214", "number": 214},
        {"task_id": 4131, "lane": "impl-list-polish", "key": "pr.docs", "value": "213", "number": 213},
    ],
    "log": [
        ev(61911, "launch", "14:15:31", "impl-frames", "claude opus 5.5 medium"),
        ev(61890, "launch", "14:13:55", "impl-trust-rules", "codex gpt-6.1-sol high"),
        ev(61901, "note", "14:07:29", "tui-frames", NOTE, owner=True),
        ev(61899, "ready", "14:07:02", "arch-plan-counter2", "counter-review 2"),
        ev(61467, "closed", "14:05:31", "rs-visual-spec", ""),
        ev(61466, "closed", "14:05:31", "arch-plan-counter", ""),
        ev(61429, "done", "14:03:49", "arch-plan-counter", "Second review done: three wording fixes, then it can go"),
        ev(61428, "ready", "14:03:43", "arch-plan-counter", "counter-review"),
        ev(61301, "decision", "13:58:40", "tui-frames", D4),
        ev(61245, "closed", "13:43:05", "arch-rust-path", ""),
        ev(61237, "done", "13:40:44", "arch-rust-path", "Report complete: migration, transport and campaign inventory"),
        ev(61216, "note", "13:35:31", "arch-rust-path", "Transport inventory drafted at the assigned report path"),
        ev(61206, "fail", "13:32:55", "rev-snapshot", "request changes: the split is not clean"),
    ],
    "spark": [14, 1, 8, 0, 6, 12, 6, 7, 13, 10, 6, 0, 0, 0, 0, 0, 0, 0, 21, 24, 8, 1, 11, 2],
}

root = lambda id, name, status, age, host="", parked=False, lanes_open=0, lanes_total=0: {
    "id": id, "name": name, "status": status, "parked": parked, "host": host, "lanes_open": lanes_open,
    "lanes_total": lanes_total, "activity_age_ms": age}
roots = [root(c["id"], c["name"], "open", c["activity_age_ms"], c["host"], c.get("parked", False), c["lanes"]["open"],
              c["lanes"]["open"] + 3 + c["id"] % 17) for c in campaigns]
roots += [root(4233, "map-tiles-polish", "open", 2 * H + 4 * M, LAPTOP, False, 2, 5), root(4040, "cache-audit", "open", 5 * H, "", False, 0, 14),
          root(3990, "font-swap", "open", 9 * H, "", False, 0, 8)]
for i, (name, host, total) in enumerate([
        ("map-tiles", LAPTOP, 9), ("landing-preview", LAPTOP, 6), ("owner-notes", "", 12), ("guest-checkout", "", 17),
        ("doc-capture", "", 21), ("checkout-states", "", 11), ("beta-pipeline", LAPTOP, 14), ("offline-spool", "", 19),
        ("search-latency", "", 7), ("fleet-accounts", "", 5), ("ranking-evals", "", 23), ("rpc-retry", "", 8),
        ("pricing-audit", LAPTOP, 10), ("triage-pilot", "", 4), ("dashboard-campaigns", "", 16), ("lead-liveness", "", 13),
        ("setup-hooks", "", 6), ("compact-format", "", 15)]):
    roots.append(root(3960 - 31 * i, name, "closed", 7 * H + i * i * 2 * H + i * 5 * H, host, False, 0, total))

DOC = """# Counter-review 2: the frames plan (rev 3)

Task 4176 (arch-plan-counter2). Read-only.

## Verdict first

**Hold the send until three wording fixes land (P1-1, P1-2, P1-3).** Rev 3 takes every point from the first review but one, which a later owner note replaced. The layout work is solid. The gaps are in the places a reader checks first: short rows, the counts, and the install path.

## Findings, ranked

### P1-1. Short rows are only specified for the first screen

The detail screens, the reply box, the key list, the full list and the reader have no short layout, and the first screen leaves room unused.

| Frame | Size | Empty rows |
|---|---|---|
| list-short | 46x30 | 7 of 30 |
| item-detail | 46x30 | 7 |
| old-data | 46x30 | 6 |

- The short list hides every second line once ten items do not fit, then leaves 7 rows empty.
- The item table has six columns; at 46 they cannot all fit, and the plan does not say which go first.
- A reader in a narrow pane opens a detail screen almost at once.

Wording fix:

1. Hide second lines one at a time: never more than one empty row while any second line is hidden.
2. Add short layouts for the detail screens, the reply box, the key list, the full list and the reader.
3. Drop a trailing `OWNER: nothing.` from shown note text.

### P1-2. The estimate and its parts disagree

The rows add up to 18-27 days. The summary says "about 20-30". The extra days have no row.

### P1-3. The install step names a build host that is not set up

Each machine builds its own files and uploads them itself:

```
release upload v2.5.0 dist/app-darwin-arm64
sha256sum dist/* > SHA256SUMS
```

### P2-5. Mark what changed since the last look

A short mark that fades in two seconds suits someone watching. Most readers look away and back. Keep a dim mark on rows that changed since the pane last had focus.

### P3-2. The busy indicator uses glyphs outside the chosen set

The dotted frames did not draw in the test capture. Pick frames from the set.

## Claims checked that hold

- Moving focus moves every attached client.
- Copy requests reach the client's own terminal.
- The pane handles batched redraws.
- The prototype passes 28 of 28 comparison cases.

## Ready to send after

P1-1, P1-2 and P1-3. The P2s are wording edits. The P3s can ride along.
"""
doc = {"id": 9560, "kind": "report", "name": "", "lane": "arch-plan-counter2", "version": 1, "body": DOC}

# `taskr slotr --json`: slotr status (schema 1) plus what taskr adds (available, host, now,
# root_id/root_name). Invented runs, campaigns and panes; no kind or priority (H1 adds them).
def holder(seq, run, campaign, task, pane, purpose, cost, anon, admitted, lease, state="running", root=None, warned=None, stopping=None):
    row = {"enqueue_seq": seq, "run": run, "campaign": campaign, "task": task, "pane": pane, "purpose": purpose, "cost_mib": cost,
           "anon_mib": anon, "admitted_at": admitted, "since": admitted, "lease_expires_at": lease, "warned_at": warned,
           "stopping_at": stopping, "state": state, "notify": "configured", "slot": 0, "cpu_usage_usec": 1200000}
    if root:
        row["root_id"], row["root_name"] = root
    return row
def waiter(seq, position, campaign, task, pane, purpose, cost, since, reason, root=None, pid=None):
    row = {"enqueue_seq": seq, "position": position, "campaign": campaign, "task": task, "pane": pane, "purpose": purpose,
           "cost_mib": cost, "since": since, "wait_reason": reason, "legacy_holder_pid": pid, "stop_claimed_by": None}
    if root:
        row["root_id"], row["root_name"] = root
    return row
TUI = (4120, "tui-frames")
slotr = {
    "schema_version": 1, "available": True, "host": "", "now": "2026-03-14T14:16:10.000Z", "events_path": "",
    "stats": {"available_mib": 9216, "total_mib": 31952.0, "psi_full_avg10": 0.42, "psi_full_avg60": 1.18, "load1": 7.9, "cores": 16},
    "last_stop": {"run": "slotr-runtime-77", "reason": "stop_psi_full_avg10", "at": "2026-03-14T14:02:31.000Z"},
    "pools": {
        "heavy": {"slots": 2, "budget": {"reserve_mib": 3072, "outstanding_mib": 1690.3, "projected_free_mib": 3429.7},
                  "holders": [holder(41, "slotr-heavy-41", "tui-frames", "4131", "wF2:p3", "cargo test --workspace", 4096, 3311.5,
                                     "2026-03-14T14:09:40.000Z", "2026-03-14T15:39:40.000Z", root=TUI)],
                  "queue": [waiter(47, 1, "search-index", "", "w3:p2", "vitest run", 3072, "2026-03-14T14:15:02.000Z", "legacy_lock", pid=48211)]},
        "runtime": {"slots": 3, "budget": {"reserve_mib": 3072, "outstanding_mib": 1690.3, "projected_free_mib": -154.3},
                    "holders": [holder(90, "slotr-runtime-90", "tui-frames", "4134", "wF2:p5", "dev stack for frames", 6144, 5420.0,
                                       "2026-03-14T13:31:10.000Z", "2026-03-14T15:01:10.000Z", root=TUI),
                                holder(93, "slotr-runtime-93", "billing-export", "4177", "w7:p1", "exporter preview", 3072, 2890.2,
                                       "2026-03-14T12:40:00.000Z", "2026-03-14T14:10:00.000Z", state="stopping", root=(4171, "billing-export"),
                                       warned="2026-03-14T14:05:00.000Z", stopping="2026-03-14T14:15:40.000Z")],
                    "queue": [waiter(95, 1, "auth-rotation", "4290", "w5:p2", "token service", 6144, "2026-03-14T14:12:30.000Z", "memory_budget", (4288, "auth-rotation")),
                              waiter(96, 2, "scratch", "", "", "one-off notebook", 2048, "2026-03-14T14:14:55.000Z", "fifo")]},
    },
}

args = sys.argv[1:]
live = "--live" in args
if live:
    args.remove("--live")
    by = {c["name"]: c for c in campaigns}
    by["search-index"]["last"]["text"] = "\u26a0\ufe0f CI red on main, \u2705 fixed in #88, \u274c 2 flaky tests left"
    by["auth-rotation"]["name"] = "auth-\u8a8d\u8a3c-rotation"
    by["billing-export"]["last"]["text"] = "\U0001f469\u200d\U0001f4bb pairing on the exporter: \u4e2d\u6587 headers fixed"
    by["docs-refresh"]["name"] = "\u2705docs-refresh"
    glance["needs_you"][0]["text"] = "\u26a0\ufe0f " + SHORT_ASK
    for l in lanes:
        if l["name"] == "impl-frames":
            l["name"] = "\u26a0\ufe0fimpl-frames-with-a-name-longer-than-its-column"
        if l["name"] == "impl-trust-rules":
            l["name"] = "impl-\u4fe1\u983c-rules"
            l["summary"] = "\u274c two rules left \U0001f468\u200d\U0001f469\u200d\U0001f467 \u5b8c\u4e86"
name = "fixture-live.json" if live else "fixture.json"
out = args[0] if args else os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", name)
text = json.dumps({"glance": glance, "campaign": campaign, "roots": roots, "doc": doc, "long_ask": long_ask,
                   "structured_asks": structured_asks, "slotr": slotr}, indent=1)
assert live or text.isascii(), "the fixture stays ASCII"
open(out, "w").write(text + "\n")
