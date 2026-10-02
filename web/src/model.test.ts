import { describe, expect, it } from "vitest";
import type { AttentionItem, MachineView, Orchestrator, StateTask } from "./api";
import {
  PROGRAM_CUES_MAX, ago, askKey, clock, cueAction, cueKey, cueOverdue, cueTone, laneAt, laneLine, markOf, merged, pageTitle, programCounts, programMore, programTally,
  short, signalWord, switched, tallyOf, umdCounts, wallOrder,
} from "./model";

const cue = (kind: string, extra: Partial<AttentionItem> = {}): AttentionItem =>
  ({ kind, severity: 0, machine: "mac", local: true, age_ms: 0, text: "t", action: "a", ...extra });
const orch = (id: number, tally?: string, tasks: StateTask[] = []): Orchestrator => ({
  id, name: "o" + id, role: "orchestrator", status: "open", created_at: "", waiting: false, unacked: 0, open_asks: 0,
  note: null, tasks, closed_tasks: 0, ...(tally ? { tally } : {}),
});
const lane = (id: number, extra: Partial<StateTask> = {}): StateTask => ({
  id, parent_id: 1, depth: 1, name: "l" + id, role: "implementer", status: "open", waiting: false, open_asks: 0, ...extra,
});
const machine = (name: string, orchs: Orchestrator[], extra: Partial<MachineView> = {}): MachineView => ({
  machine: name, local: true, last_push_age_ms: 0, stale: false,
  state: { now: "", version: "", owner_asks: [], orchestrators: orchs, activity: [], closed: [], milestones: [], attention: [] },
  ...extra,
});

describe("time", () => {
  it("renders short ages", () => {
    expect([0, 9_000, 45_000, 60_000, 59 * 60_000, 3 * 3600_000, 50 * 3600_000].map(short)).toEqual(["now", "now", "45s", "1m", "59m", "3h", "2d"]);
  });
  it("ages from the server clock, never negative", () => {
    const now = Date.parse("2026-09-29T12:00:00.000Z");
    expect(ago("2026-09-29T11:48:00.000Z", now)).toBe("12m ago");
    expect(ago("2026-09-29T12:00:05.000Z", now)).toBe("just now");
    expect(ago(undefined, now)).toBe("");
  });
  it("prints the viewer's clock", () => {
    expect(clock("2026-09-29T21:05:00.000Z", 0)).toBe("21:05");
    expect(clock("2026-09-29T21:05:00.000Z", -120)).toBe("23:05");
  });
});

describe("PROGRAM", () => {
  const items = [cue("owner_ask", { ask_id: 7 }), cue("owner_ask", { ask_id: 8, local: false, node_id: "n1" }), cue("lane_failed"), cue("work_waiting"), cue("work_waiting")];
  it("counts red and amber", () => {
    expect(programCounts(items)).toEqual({ red: 3, amber: 2 });
    expect(programTally({ red: 0, amber: 2 })).toBe("amber");
    expect(programTally({ red: 0, amber: 0 })).toBe("off");
    expect(pageTitle({ red: 3, amber: 2 })).toBe("(3) taskr");
    expect(pageTitle({ red: 0, amber: 0 })).toBe("taskr");
  });
  it("caps the rundown at six and counts each omitted kind without changing attention", () => {
    const shown = Array.from({ length: PROGRAM_CUES_MAX }, () => cue("owner_ask"));
    const items = [...shown, cue("machine_stale"), cue("daemon_unhealthy"), cue("machine_stale"), cue("work_waiting")];
    expect(PROGRAM_CUES_MAX).toBe(6);
    expect(programMore(shown)).toBe("");
    expect(programMore(items)).toBe("+4 more: Machine silent 2, Daemon 1, Work waiting 1");
    expect(programCounts(items)).toEqual({ red: 9, amber: 1 });
    expect(items).toHaveLength(10);
  });
  it("keeps a short answer destination and the observed dialog action under a wait lease", () => {
    const c = cue("owner_ask", { pane_id: "w1:p1", asker_waiting: true,
      also: [{ kind: "lane_blocked", age_ms: 0, text: "dialog", action: "If the dialog is not this question, answer it in its pane" }] });
    expect(cueAction(c)).toBe("Answer in w1:p1 · check dialog");
    expect(cueAction(cue("lane_failed"))).toBe("Read report");
    expect(cueAction(cue("future", { action: "Next step" }))).toBe("Next step");
  });
  it("tones and hatches cues", () => {
    expect(cueTone("work_waiting")).toBe("amber");
    expect(cueTone("lane_blocked")).toBe("red");
    expect(["work_waiting", "machine_stale", "daemon_unhealthy", "lane_failed"].map(cueOverdue)).toEqual([true, true, true, false]);
  });
  it("keys cues stably and names folded lane states", () => {
    expect(cueKey(cue("owner_ask", { ask_id: 8, local: false, node_id: "n1" }))).toBe("peer:n1:8");
    expect(cueKey(cue("lane_failed", { lane: { id: 4, name: "l" }, orchestrator: { id: 1, name: "o" } }))).toBe("lane_failed:local:4");
    expect(cueKey(cue("machine_stale", { local: false, machine: "box" }))).toBe("machine_stale:peer:box:");
    expect(["lane_blocked", "lane_failed", "lane_missing"].map(signalWord)).toEqual(["blocked", "failed", "pane gone"]);
  });
});

describe("the wall", () => {
  it("orders lit monitors first", () => {
    expect(wallOrder([orch(1), orch(2, "green"), orch(3, "red"), orch(4, "amber"), orch(5, "bogus")]).map((o) => o.id)).toEqual([3, 4, 2, 1, 5]);
    expect(tallyOf(orch(9, "bogus"))).toBe("off");
  });
  it("marks lanes and dates them", () => {
    const blocked = lane(1, { mark: "blocked", observed_at: "A", last_event: { kind: "note", at: "B", age_ms: 0 } });
    const done = lane(2, { mark: "done", last_event: { kind: "done", summary: "all good", at: "C", age_ms: 0 } });
    const planned = lane(3, { mark: "planned", next: { text: "later", at: "", age_ms: 0 } });
    expect([laneAt(blocked), laneAt(done), laneAt(planned)]).toEqual(["A", "C", undefined]);
    expect(laneLine(done)).toEqual({ flag: false, text: "all good" });
    expect(laneLine(planned)).toEqual({ flag: true, text: "later" });
    expect(laneLine(blocked)).toBeNull();
    expect(markOf(lane(4))).toBe("working");
  });
  it("counts the label strip", () => {
    const o = { ...orch(1, "red", [lane(1, { mark: "done" }), lane(2, { mark: "planned" }), lane(3, { mark: "working" })]), closed_tasks: 2, pane_id: "w1:p1" };
    expect(umdCounts(o)).toBe("1 lane · 1 planned · 2 closed · w1:p1");
  });
  it("flashes a tally only when it changes to lit", () => {
    const first = switched(new Map(), [machine("mac", [orch(1, "red"), orch(2)])]);
    expect([...first.changed]).toEqual([]);
    const second = switched(first.next, [machine("mac", [orch(1, "red"), orch(2, "green")])]);
    expect([...second.changed]).toEqual(["local:2"]);
    const third = switched(second.next, [machine("mac", [orch(1, "off"), orch(2, "green")])]);
    expect([...third.changed]).toEqual([]);
  });
});

describe("the wire", () => {
  it("merges machines newest first with labels", () => {
    const a = machine("mac", []);
    a.state.activity = [{ id: 1, kind: "note", at: "2026-09-29T10:00:00.000Z", age_ms: 0, task: { id: 1, name: "o" }, root_id: 1, root_name: "o" }];
    const b = machine("box", [], { local: false, node_id: "n" });
    b.state.activity = [{ id: 1, kind: "ready", at: "2026-09-29T11:00:00.000Z", age_ms: 0, task: { id: 2, name: "l" }, root_id: 1, root_name: "p" }];
    expect(merged([a, b], "activity", 10).map((x) => x.machine + ":" + x.kind)).toEqual(["box:ready", "mac:note"]);
  });
});

describe("the Go-written fixture", async () => {
  const { fixture } = await import("./fixtures/check");
  it("counts its attention", () => {
    const c = programCounts(fixture.attention);
    expect(c.amber).toBeGreaterThan(0);
    expect(c.red + c.amber).toBe(fixture.attention.length);
  });
  it("finds every owner ask cue in needs_you, with its orchestrator's pane as the action", () => {
    const asks = fixture.attention.filter((a) => a.kind === "owner_ask");
    expect(asks.length).toBeGreaterThan(0);
    for (const a of asks) {
      expect(fixture.needs_you.some((n) => askKey(n, n.id) === askKey(a, a.ask_id ?? 0))).toBe(true);
      expect(a.action.startsWith("Answer in " + (a.orchestrator?.name ?? "") + "'s pane")).toBe(true);
    }
  });
  it("folds a lane's own cues into its ask, without repeating the ask", () => {
    const folded = fixture.attention.filter((a) => a.also?.length);
    expect(folded.length).toBeGreaterThan(0);
    for (const a of folded) for (const x of a.also ?? []) {
      expect(x.text).not.toContain(a.text);
      expect(fixture.attention.some((c) => c.kind === x.kind && c.lane?.id === a.lane?.id)).toBe(false);
    }
    const texts = fixture.attention.map((a) => a.text);
    expect(new Set(texts).size).toBe(texts.length);
  });
  it("marks and tallies every campaign", () => {
    for (const m of fixture.machines) for (const o of m.state.orchestrators) {
      expect(["red", "amber", "green", "off"]).toContain(tallyOf(o));
      for (const t of o.tasks) expect(t.mark).toBeDefined();
    }
  });
});
