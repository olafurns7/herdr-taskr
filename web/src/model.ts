// The view model: pure functions from /api/state to what the wall shows.
// No DOM, no clock of its own: callers pass `now` (server time, ms).
import type { AttentionItem, MachineView, Orchestrator, StateTask } from "./api";

export type Tally = "red" | "amber" | "green" | "off";
export type Mark = "done" | "failed" | "working" | "planned" | "ready" | "blocked" | "missing";
export type Icon = Mark | "flag" | "note" | "handover" | "adopt" | "decision" | "ref" | "open" | "inbox" | "campaign" | "activity" | "machine" | "search" | "back";

/** A milestone within this window keeps its tally green (dashboard.go milestoneLitFor). */
export const FRESH_MS = 10 * 60 * 1000;

// ---- time -------------------------------------------------------------------

/** short renders an age: "now", "40s", "12m", "3h", "2d". */
export function short(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 10) return "now";
  if (s < 60) return s + "s";
  const m = Math.floor(s / 60);
  if (m < 60) return m + "m";
  const h = Math.floor(m / 60);
  if (h < 48) return h + "h";
  return Math.floor(h / 24) + "d";
}

export function ageOf(ts: string | undefined, now: number): number {
  if (!ts) return 0;
  const t = Date.parse(ts);
  return Number.isNaN(t) ? 0 : Math.max(0, now - t);
}

/** ago is short() as a phrase: "just now", "12m ago". */
export function ago(ts: string | undefined, now: number): string {
  if (!ts) return "";
  const a = short(ageOf(ts, now));
  return a === "now" ? "just now" : a + " ago";
}

/** clock is HH:MM in the viewer's zone. */
export function clock(ts: string, tzOffsetMin = new Date(ts).getTimezoneOffset()): string {
  const d = new Date(Date.parse(ts) - tzOffsetMin * 60000);
  return String(d.getUTCHours()).padStart(2, "0") + ":" + String(d.getUTCMinutes()).padStart(2, "0");
}

/** noteTime is a short weekday and HH:MM in the viewer's zone. */
export function noteTime(ts: string, tzOffsetMin = new Date(ts).getTimezoneOffset()): string {
  const d = new Date(Date.parse(ts) - tzOffsetMin * 60000);
  return ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][d.getUTCDay()] + " " + clock(ts, tzOffsetMin);
}

// ---- PROGRAM ----------------------------------------------------------------

export function askKey(a: { local: boolean; node_id?: string; machine?: string }, id: number): string {
  return (a.local ? "local" : "peer:" + (a.node_id ?? a.machine ?? "")) + ":" + id;
}

/** programCounts: red cues (every kind but work waiting) and amber ones. */
export function programCounts(items: AttentionItem[]): { red: number; amber: number } {
  let red = 0;
  let amber = 0;
  for (const c of items) {
    if (c.kind === "work_waiting") amber++;
    else red++;
  }
  return { red, amber };
}

export function programTally(c: { red: number; amber: number }): Tally {
  return c.red ? "red" : c.amber ? "amber" : "off";
}

export function pageTitle(c: { red: number; amber: number }, newNotes = 0): string {
  const counts = [c.red ? String(c.red) : c.amber ? "·" : "", newNotes ? newNotes + " new" : ""].filter(Boolean);
  return counts.length ? "(" + counts.join(", ") + ") taskr" : "taskr";
}

const CUE_KIND: Record<string, string> = {
  owner_ask: "Owner ask",
  lane_blocked: "Blocked",
  lane_failed: "Failed",
  lane_missing: "Pane gone",
  machine_stale: "Machine silent",
  daemon_unhealthy: "Daemon",
  work_waiting: "Work waiting",
};

export function cueLabel(kind: string): string {
  return CUE_KIND[kind] ?? kind.replace(/_/g, " ");
}

export const PROGRAM_CUES_MAX = 6;

export function programMore(items: AttentionItem[]): string {
  const hidden = items.slice(PROGRAM_CUES_MAX);
  const counts = new Map<string, number>();
  for (const c of hidden) counts.set(c.kind, (counts.get(c.kind) ?? 0) + 1);
  return hidden.length ? "+" + hidden.length + " more: " + [...counts].map(([kind, n]) => cueLabel(kind) + " " + n).join(", ") : "";
}

export function cueAction(c: AttentionItem): string {
  const action = {
    owner_ask: "Answer in " + (c.pane_id || c.orchestrator?.name || c.machine),
    lane_blocked: "Check pane", lane_failed: "Read report", lane_missing: "Relaunch lane",
    machine_stale: "Check machine", daemon_unhealthy: "Check daemon", work_waiting: "Check orchestrator",
  }[c.kind] ?? c.action;
  const also = (c.also ?? []).map((x) => x.kind === "lane_blocked" ? "check dialog" : x.kind === "lane_failed" ? "read report" : "relaunch lane");
  return [action, ...also].join(" · ");
}

/** cueKey names a cue across polls. */
export function cueKey(c: AttentionItem): string {
  if (c.kind === "owner_ask") return askKey(c, c.ask_id ?? 0);
  return c.kind + ":" + (c.local ? "local" : "peer:" + (c.node_id ?? c.machine)) + ":" + (c.lane?.id ?? c.orchestrator?.id ?? "");
}

/** signalWord is a folded lane cue as a tag on its ask's line. */
export function signalWord(kind: string): string {
  return kind === "lane_missing" ? "pane gone" : cueLabel(kind).toLowerCase();
}

/** cueTone: amber for work waiting on an orchestrator, red for the rest. */
export function cueTone(kind: string): "red" | "amber" {
  return kind === "work_waiting" ? "amber" : "red";
}

/** overdue cues wear the zebra: late work and silent machines or daemons. */
export function cueOverdue(kind: string): boolean {
  return kind === "work_waiting" || kind === "machine_stale" || kind === "daemon_unhealthy";
}

// ---- the wall ---------------------------------------------------------------

const TALLIES: Tally[] = ["red", "amber", "green", "off"];

export function tallyOf(o: Orchestrator): Tally {
  return (TALLIES as string[]).includes(o.tally ?? "") ? (o.tally as Tally) : "off";
}

export const TALLY_WORD: Record<Tally, string> = { red: "Needs you", amber: "Work waiting", green: "Milestone", off: "Quiet" };

/** wallOrder: lit monitors first (red, amber, green), then by id. */
export function wallOrder(orchs: Orchestrator[]): Orchestrator[] {
  return orchs.slice().sort((a, b) => TALLIES.indexOf(tallyOf(a)) - TALLIES.indexOf(tallyOf(b)) || a.id - b.id);
}

const MARKS: Mark[] = ["done", "failed", "working", "planned", "ready", "blocked", "missing"];

export function markOf(t: StateTask): Mark {
  return (MARKS as string[]).includes(t.mark ?? "") ? (t.mark as Mark) : "working";
}

export const MARK_WORD: Record<Mark, string> = {
  done: "done", failed: "failed", working: "working", planned: "planned", ready: "ready", blocked: "blocked", missing: "pane gone",
};

/** laneAt is the moment a lane's state dates from. */
export function laneAt(t: StateTask): string | undefined {
  const m = markOf(t);
  if (m === "planned") return undefined;
  if (m === "blocked" || m === "missing") return t.observed_at;
  return t.last_event?.at;
}

/** laneLine is the one line under a lane: its next step, else its report. */
export function laneLine(t: StateTask): { flag: boolean; text: string } | null {
  if (t.next) return { flag: true, text: t.next.text };
  const m = markOf(t);
  if ((m === "ready" || m === "failed" || m === "done") && t.last_event?.summary) return { flag: false, text: t.last_event.summary };
  return null;
}

/** umdCounts is the label strip's right side. */
export function umdCounts(o: Orchestrator): string {
  const marks = o.tasks.map(markOf);
  const live = marks.filter((m) => m !== "done" && m !== "planned").length;
  const out = [live + (live === 1 ? " lane" : " lanes")];
  const planned = marks.filter((m) => m === "planned").length;
  if (planned) out.push(planned + " planned");
  if (o.closed_tasks) out.push(o.closed_tasks + " closed");
  if (o.tasks_truncated) out.push(o.tasks_truncated + " not shown");
  if (o.pane_id) out.push(o.pane_id);
  return out.join(" · ");
}

export function tileKey(m: MachineView, o: Orchestrator): string {
  return (m.local ? "local" : m.node_id ?? m.machine) + ":" + o.id;
}

/** switched lists the tiles whose tally changed since the last poll. */
export function switched(prev: ReadonlyMap<string, Tally>, machines: MachineView[]): { next: Map<string, Tally>; changed: Set<string> } {
  const next = new Map<string, Tally>();
  const changed = new Set<string>();
  for (const m of machines) {
    for (const o of m.state.orchestrators) {
      const k = tileKey(m, o);
      const t = tallyOf(o);
      next.set(k, t);
      const p = prev.get(k);
      if (p !== undefined && p !== t && t !== "off") changed.add(k);
    }
  }
  return { next, changed };
}

// ---- cue log and wire -------------------------------------------------------

const LOG: Record<string, { word: string; icon: Icon }> = {
  ready: { word: "ready", icon: "ready" },
  done: { word: "done", icon: "done" },
  fail: { word: "failed", icon: "failed" },
  handover: { word: "handover", icon: "handover" },
  adopt: { word: "adopted", icon: "adopt" },
  decision: { word: "decision", icon: "decision" },
  note: { word: "status", icon: "note" },
  ref: { word: "ref", icon: "ref" },
};

export function logKind(kind: string): { word: string; icon: Icon } {
  return LOG[kind] ?? { word: kind, icon: "note" };
}

/** merged lists one field of every machine's state, newest first. */
export function merged<K extends "activity" | "closed">(machines: MachineView[], field: K, cap: number): (MachineView["state"][K][number] & { machine: string })[] {
  const out: (MachineView["state"][K][number] & { machine: string })[] = [];
  for (const m of machines) for (const x of m.state[field] ?? []) out.push({ ...x, machine: m.machine });
  const at = (x: { at?: string; closed_at?: string }) => Date.parse(x.at ?? x.closed_at ?? "");
  out.sort((x, y) => at(y) - at(x));
  return out.slice(0, cap);
}

export function plural(n: number, one: string, many: string): string {
  return n + " " + (n === 1 ? one : many);
}
