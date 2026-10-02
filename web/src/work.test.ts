import { expect, it } from "vitest";
import { fixture } from "./fixtures/check";
import { filterItems, machineKey, workItems } from "./work";

it("keeps colliding machine/task ids separate and searches full detail without hiding other signals", () => {
  const s = structuredClone(fixture);
  const peer = structuredClone(s.machines[0]!);
  peer.local = false; peer.node_id = "peer-collision"; peer.machine = "another-machine";
  peer.state.orchestrators[0]!.next = { text: "Full long next step with uniquely-findable-tail", at: s.now, age_ms: 0 };
  s.machines.push(peer);
  const ask = structuredClone(s.attention[0]!);
  ask.local = false; ask.node_id = peer.node_id; ask.machine = peer.machine;
  ask.also = [{ kind: "lane_failed", text: "full folded signal", action: "inspect full report", command: "taskr log --as 3", report: "/tmp/unique-report-tail.md", age_ms: 0 }];
  s.attention.push(ask);
  const data = workItems(s);
  const all = [...data.inbox, ...data.campaigns, ...data.lanes, ...data.milestones];
  expect(new Set(all.map(x => x.key)).size).toBe(all.length);
  expect(data.inbox).toHaveLength(s.attention.length);
  expect(filterItems(data.inbox, machineKey(peer), "unique-report-tail")).toHaveLength(1);
  expect(filterItems(data.campaigns, machineKey(peer), "uniquely-findable-tail")).toHaveLength(1);
  expect(data.inbox.find(x => x.machineKey === machineKey(peer))?.detail).toEqual({ kind: "cue", cue: ask });
  expect(filterItems(data.inbox, "", "no such result")).toHaveLength(0);
  expect(data.inbox).toHaveLength(s.attention.length);
  expect(new Set(data.campaigns.filter(x => x.title === "host-a-orch").map(x => x.key)).size).toBe(2);
});

it("keeps complete next/report/truncation data in the campaign and lane detail", () => {
  const s = structuredClone(fixture);
  const o = s.machines[0]!.state.orchestrators[0]!;
  o.tasks_truncated = 4;
  o.next = { text: "full next\nsecond paragraph", at: s.now, age_ms: 0 };
  o.tasks[0]!.report_path = "/tmp/full-report.md";
  const data = workItems(s);
  const root = data.campaigns.find(x => x.title === o.name)!;
  expect(root.preview).toBe(o.next.text);
  expect(root.detail.kind === "campaign" && root.detail.campaign.tasks_truncated).toBe(4);
  expect(data.lanes.find(x => x.title === o.tasks[0]!.name)?.detail).toEqual({ kind: "campaign", machine: s.machines[0], campaign: o, lane: o.tasks[0] });
});
