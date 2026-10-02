import type { AttentionItem, MachineView, Milestone, Orchestrator, StateTask, StateView } from "./api";
import { cueKey, cueLabel, cueTone, laneAt, logKind, MARK_WORD, markOf, tallyOf, TALLY_WORD, tileKey, umdCounts, wallOrder, type Icon, type Tally } from "./model";

export type WorkDetail =
  | { kind: "cue"; cue: AttentionItem }
  | { kind: "campaign"; machine: MachineView; campaign: Orchestrator; lane?: StateTask }
  | { kind: "milestone"; milestone: Milestone };

export interface WorkItem {
  key: string;
  machineKey: string;
  machine: string;
  title: string;
  context: string;
  preview: string;
  status: string;
  icon: Icon;
  tone: Tally;
  at?: string;
  detail: WorkDetail;
  search: string;
}

export function machineKey(m: { local: boolean; node_id?: string; machine: string }): string {
  return m.local ? "local" : "peer:" + (m.node_id ?? m.machine);
}

const item = (x: Omit<WorkItem, "search">, extra = ""): WorkItem =>
  ({ ...x, search: [x.title, x.context, x.machine, x.preview, x.status, extra].join(" ").toLowerCase() });

export function campaignItem(m: MachineView, o: Orchestrator, t?: StateTask): WorkItem {
  const mark = t ? markOf(t) : null;
  const tone = t ? (mark === "blocked" || mark === "failed" || mark === "missing" ? "red" : mark === "ready" || mark === "done" ? "green" : t.waiting ? "amber" : "off") : tallyOf(o);
  return item({
    key: "campaign:" + tileKey(m, o) + (t ? ":lane:" + t.id : ""),
    machineKey: machineKey(m), machine: m.machine, title: t?.name ?? o.name,
    context: t ? o.name + " · " + t.role : umdCounts(o),
    preview: t ? t.next?.text ?? t.last_event?.summary ?? "" : o.next?.text ?? o.note?.text ?? "No next step recorded",
    status: t ? (t.waiting && mark === "working" ? "Waiting" : MARK_WORD[mark!]) : tone === "off" ? (o.waiting ? "Waiting" : "Open") : TALLY_WORD[tone],
    icon: mark ?? "campaign", tone,
    at: t ? laneAt(t) : o.note?.at ?? o.last_event?.at,
    detail: { kind: "campaign", machine: m, campaign: o, ...(t ? { lane: t } : {}) },
  }, [t?.report_path, t?.last_event?.summary, o.note?.text, o.next?.text, ...(o.refs ?? []).map(r => r.key + "=" + r.value)].filter(Boolean).join(" "));
}

export function workItems(s: StateView) {
  const inbox = s.attention.map(c => item({
    key: "cue:" + cueKey(c), machineKey: machineKey(c), machine: c.machine,
    title: c.lane?.name ?? c.orchestrator?.name ?? c.machine,
    context: c.lane ? c.orchestrator?.name ?? "" : "", preview: c.text,
    status: cueLabel(c.kind), icon: c.kind === "owner_ask" ? "inbox" : c.kind === "work_waiting" ? "ready" : c.kind === "lane_failed" ? "failed" : c.kind === "lane_missing" ? "missing" : "blocked",
    tone: cueTone(c.kind), at: c.since, detail: { kind: "cue", cue: c },
  }, [c.action, c.command, c.report, ...(c.also ?? []).flatMap(x => [x.text, x.action, x.command, x.report])].filter(Boolean).join(" ")));
  const campaigns = s.machines.flatMap(m => wallOrder(m.state.orchestrators).map(o => campaignItem(m, o)));
  const lanes = s.machines.flatMap(m => m.state.orchestrators.flatMap(o => o.tasks.map(t => campaignItem(m, o, t))));
  const milestones = s.milestones.map(e => {
    const machine = s.machines.find(m => e.node_id ? m.node_id === e.node_id : m.machine === e.machine) ?? s.machines.find(m => m.local);
    const identity = machine ? machineKey(machine) : "peer:" + (e.node_id ?? e.machine ?? "");
    return item({
      key: "milestone:" + identity + ":" + e.id, machineKey: identity, machine: e.machine ?? machine?.machine ?? s.machine,
      title: e.lane ?? e.orchestrator, context: e.lane ? e.orchestrator : "", preview: e.text,
      status: logKind(e.kind).word, icon: logKind(e.kind).icon, tone: e.kind === "fail" ? "red" : "off", at: e.at,
      detail: { kind: "milestone", milestone: e },
    });
  });
  return { inbox, campaigns, lanes, milestones };
}

export function filterItems(items: WorkItem[], machine: string, query: string): WorkItem[] {
  const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  return items.filter(x => (!machine || x.machineKey === machine) && words.every(w => x.search.includes(w)));
}
