#!/usr/bin/env python3
"""Write the synthetic fixture the frames are drawn from. Usage: mkfixture.py [OUT]

Nothing here comes from a ledger: campaign names, ask texts, task ids, hosts and PR
numbers are invented. The shapes follow `taskr --json glance` plus the fields P1a and
P1b add, and the lengths and edge cases follow real use: a 400-character ask from another
machine, a campaign with 29 lanes and a sub-orchestrator, a stale host, a parked
campaign, a lead that is idle with results, a note that says only "OWNER: nothing.".
Text stays ASCII so every glyph in a frame is one the view drew.
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
    camp(4120, "tui-frames", "working", False, (3, 1, 6), 8 * M, "busy", "Counter-review written: not ready to send; three text fixes make it sendable", "done",
         note="OWNER: nothing. NOW: plan rev 3.1 sent; the trust rules and the first frames are in flight."),
    camp(4031, "infra-layout", "idle", False, (0, 0, 0), 12 * M, "busy", "cache-audit lead root=4040 pane=w41:p1: campaign complete, docs PR merged", "note"),
    camp(4215, "model-evals", "working", False, (5, 2, 9), 14 * M, "early", "matrix run A finished"),
    camp(4066, "api-pagination", "idle", True, (1, 0, 1), 54 * M, "early", "S4 cursor split merged; contract tests green on both hosts", "done", note="OWNER: nothing."),
    camp(3254, "legacy-importer", "idle", False, (0, 0, 0), 3 * H + 9 * M, "idle", "Held by the owner until the import budget is decided.", "note",
         parked=True, park_age_ms=3 * H),
]
campaigns[-1]["spark"] = [0] * 24

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
         summary="Counter-review 2 written: not ready to send until three text fixes land (P1-1 narrow layouts, P1-2 numbers, P1-3 release path)."),
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
    "goal": ["tui-frames: a lightweight terminal inbox and campaign status for the owner",
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
        ev(61429, "done", "14:03:49", "arch-plan-counter", "Counter-review written: not ready to send; three text fixes make it sendable"),
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

DOC = """# Counter-review 2: the phase 3 plan (rev 3)

Task 4176 (arch-plan-counter2). Read-only.

## Verdict first

**Not ready to send until three text fixes land (P1-1, P1-2, P1-3).** Rev 3 honours every fix from the first review except one that the owner's later input overrides. The research behind it is sound. What is wrong is at the edges the owner will touch first.

## Findings, ranked

### P1-1. The 46-column spec covers the glance only

The campaign view, answer dialog, help, all-campaigns list and pager have no narrow layout, and the glance itself wastes a quarter of the pane.

| Frame | Size | Blank rows |
|---|---|---|
| glance-narrow | 46x30 | 7 of 30 |
| row-detail | 46x30 | 7 |
| stale | 46x30 | 6 |

- The narrow glance folds every campaign's second line because 10 campaigns x 2 lines do not fit, then leaves 7 rows empty.
- The lanes table has six columns; at 46 columns they cannot fit, and nothing says which columns go.
- The owner will open a campaign from the 46-column pane within the first minute.

Fix to the plan text:

1. Fold progressively, not all-or-nothing: a frame never shows more than one blank row while any second line is folded.
2. Add narrow layouts for the campaign view, the answer dialog, help, the all-campaigns list and the pager.
3. Strip the `OWNER: nothing.` segment from displayed note text.

### P1-2. The numbers the owner is asked to approve do not agree

The phase rows sum to 26-39.5 weeks. The table says "about 28-45". Nothing names the extra 2-5.5.

### P1-3. The second build runs over a remote target that does not exist

Each host builds its own targets and uploads its own assets:

```
release upload v2.5.0 dist/app-darwin-arm64
sha256sum dist/* > SHA256SUMS
```

### P2-5. A glance view needs "new since you looked"

A 2-second gutter mark and a 300 ms flash are for someone who stares. The owner glances between other things. Keep a dim mark on rows that changed since the pane was last focused.

### P3-2. The spinner uses glyphs outside the declared set

The braille frames did not render in the lane's own picture. Pick in-set frames.

## Claims checked that hold

- A focus call moves every attached client.
- Clipboard writes are forwarded to the client's own terminal.
- Synchronized output is handled by the pane emulator.
- The spike passes 28 of 28 differential cases.

## Ready to send after

P1-1, P1-2 and P1-3. The P2s are text edits for the orchestrator. The P3s can ride along.
"""
doc = {"id": 9560, "kind": "report", "name": "", "lane": "arch-plan-counter2", "version": 1, "body": DOC}

out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "fixture.json")
text = json.dumps({"glance": glance, "campaign": campaign, "roots": roots, "doc": doc}, indent=1)
assert text.isascii(), "the fixture stays ASCII"
open(out, "w").write(text + "\n")
