import { expect, it } from "vitest";
import type { VNode } from "preact";
import { fixture } from "./fixtures/check";
import { inboxCount, inboxScope, ownerNoteSummary, workItems } from "./work";
import { noteTime, pageTitle } from "./model";
import { readOwnerNotes, markOwnerNotesRead, OWNER_NOTES_READ_KEY } from "./owner-notes";
import { InboxGroups } from "./components/Dashboard";
import { Detail } from "./components/Detail";
import { Campaign } from "./components/CampaignPage";
import type { StateView } from "./api";
import type { CampaignView } from "./campaign-api";

const notes = [
  { id: 30, root_id: 1, root_name: "release", text: "Review the release\nFull newest note", at: "2026-10-04T12:00:00Z", age_ms: 0 },
  { id: 20, root_id: 2, root_name: "docs", text: "Read the guide", at: "2026-10-04T11:00:00Z", age_ms: 3600000 },
  { id: 10, root_id: 1, root_name: "release", text: "Earlier note\nComplete earlier text", at: "2026-10-04T10:00:00Z", age_ms: 7200000 },
];

it("groups notes into one neutral row per root, newest first, retaining complete earlier notes", () => {
  const s = { ...fixture, owner_notes: [notes[2]!, notes[1]!, notes[0]!] };
  const data = workItems(s);
  expect(data.ownerNotes.map(x => [x.title, x.preview, x.tone, x.status])).toEqual([
    ["release", "Review the release", "off", "New"], ["docs", "Read the guide", "off", "New"],
  ]);
  expect(data.ownerNotes[0]!.detail).toEqual({ kind: "owner-notes", notes: [notes[0], notes[2]] });
  expect(workItems({ ...s, owner_notes: [{ ...notes[0]!, id: 40 }, ...notes] }).ownerNotes[0]!.key).toBe(data.ownerNotes[0]!.key);
});

it("accepts an older hub without owner_notes", () => {
  const s = { ...fixture };
  delete s.owner_notes;
  expect(workItems(s).ownerNotes).toEqual([]);
});

it("marks only newer rows new and preserves read rows", () => {
  const rows = workItems({ ...fixture, owner_notes: notes }, 20).ownerNotes;
  expect(rows.map(x => [x.isNew, x.status])).toEqual([[true, "New"], [false, "Read"]]);
});

it("persists one browser marker, starts new on read failure, and keeps the session marker on write failure", () => {
  const values = new Map<string, string>();
  const storage = () => ({ getItem: (key: string) => values.get(key) ?? null, setItem: (key: string, value: string) => { values.set(key, value); } });
  expect(readOwnerNotes(storage)).toBe(0);
  expect(markOwnerNotesRead(30, storage)).toBe(30);
  expect(values.get(OWNER_NOTES_READ_KEY)).toBe("30");
  expect(readOwnerNotes(storage)).toBe(30);
  const throws = () => { throw new Error("Storage unavailable"); };
  expect(readOwnerNotes(throws)).toBe(0);
  expect(markOwnerNotesRead(30, throws)).toBe(30);
  const denied = () => ({ getItem: () => { throw new Error("Denied"); }, setItem: () => { throw new Error("Quota"); } });
  expect(readOwnerNotes(denied)).toBe(0);
  expect(markOwnerNotesRead(30, denied)).toBe(30);
  for (const invalid of ["bad", "-1", "Infinity", "1.5"]) {
    values.set(OWNER_NOTES_READ_KEY, invalid);
    expect(readOwnerNotes(storage)).toBe(0);
  }
});

it("keeps new note counts distinct from attention and waiting in the page title", () => {
  expect(pageTitle({ red: 0, amber: 0 }, 2)).toBe("(2 new) taskr");
  expect(pageTitle({ red: 3, amber: 0 }, 2)).toBe("(3, 2 new) taskr");
  expect(pageTitle({ red: 0, amber: 1 }, 2)).toBe("(·, 2 new) taskr");
  expect(pageTitle({ red: 0, amber: 0 }, 0)).toBe("taskr");
});

it("counts new campaign rows and marks through every snapshot note without moving backwards", () => {
  const s = { ...fixture, owner_notes: [notes[2]!, notes[0]!, notes[1]!] };
  expect(ownerNoteSummary(s, 0)).toEqual({ newRows: 2, markThrough: 30 });
  expect(ownerNoteSummary(s, 20)).toEqual({ newRows: 1, markThrough: 30 });
  expect(ownerNoteSummary(s, 40)).toEqual({ newRows: 0, markThrough: 40 });
  expect(ownerNoteSummary({ ...s, owner_notes: [] }, 20)).toEqual({ newRows: 0, markThrough: 20 });
  const older: StateView = { ...s };
  delete older.owner_notes;
  expect(ownerNoteSummary(older, 0)).toEqual({ newRows: 0, markThrough: 0 });
});

it("counts attention plus one row per root for Inbox and All, and hides notes in attention and waiting scopes", () => {
  const data = workItems({ ...fixture, owner_notes: notes });
  expect(inboxCount(data)).toBe(fixture.attention.length + 2);
  expect(inboxScope(data, "all")).toEqual({ inbox: data.inbox, ownerNotes: data.ownerNotes });
  for (const [scope, tone] of [["attention", "red"], ["waiting", "amber"]] as const) {
    const scoped = inboxScope(data, scope);
    expect(scoped.inbox.length).toBeGreaterThan(0);
    expect(scoped.inbox).toEqual(data.inbox.filter(x => x.tone === tone));
    expect(scoped.ownerNotes).toEqual([]);
  }
  expect(inboxCount(workItems({ ...fixture, attention: [], owner_notes: notes }))).toBe(2);
});

function children(node: unknown): VNode<Record<string, unknown>>[] {
  if (Array.isArray(node)) return node.flatMap(children);
  if (!node || typeof node !== "object" || !("props" in node)) return [];
  const vnode = node as VNode<Record<string, unknown>>;
  return [vnode, ...children(vnode.props.children)];
}

it("renders For you under Owner asks and shows up-to-date only without notes or attention", () => {
  const data = workItems({ ...fixture, owner_notes: notes });
  const props = { inbox: data.inbox, ownerNotes: data.ownerNotes, total: data.inbox.length + data.ownerNotes.length, unavailable: false, selected: null, now: Date.parse(fixture.now), open: () => {}, markRead: () => {} };
  const titles = (extra: Partial<typeof props>) => children(InboxGroups({ ...props, ...extra })).map(x => x.props.title).filter(Boolean);
  expect(titles({}).slice(0, 2)).toEqual(["Owner asks", "For you"]);
  expect(titles({ inbox: [] })).toEqual(["For you"]);
  expect(titles({ inbox: [], ownerNotes: [], total: 0 })).toEqual(["You're up to date"]);
  expect(titles({ inbox: [], ownerNotes: [] })).toEqual(["No matching inbox items"]);
  expect(titles({ inbox: [], ownerNotes: [], total: 0, unavailable: true })).toEqual(["No inbox items in this snapshot"]);
});

it("shows Mark read only while the For you group contains a new row", () => {
  const actions = (readThrough: number) => {
    const data = workItems({ ...fixture, owner_notes: notes }, readThrough);
    const nodes = children(InboxGroups({ inbox: [], ownerNotes: data.ownerNotes, total: inboxCount(data), unavailable: false, selected: null, now: 0, open: () => {}, markRead: () => {} }));
    return nodes.find(x => x.props.title === "For you")?.props.action;
  };
  expect(actions(0)).toBeTruthy();
  expect(actions(20)).toBeTruthy();
  expect(actions(30)).toBeUndefined();
});

it("keeps full owner-note detail and each timestamp in newest-first order", () => {
  const row = workItems({ ...fixture, owner_notes: notes }).ownerNotes[0]!;
  const nodes = children(Detail({ item: row, now: Date.parse(fixture.now), open: () => {} }));
  expect(nodes.filter(x => x.type === "time").map(x => x.props.dateTime)).toEqual([notes[0]!.at, notes[2]!.at]);
  expect(nodes.filter(x => x.type === "time").map(x => (x.props.children as unknown[])[0])).toEqual([noteTime(notes[0]!.at), noteTime(notes[2]!.at)]);
  expect(nodes.filter(x => x.type === "p").map(x => x.props.children)).toContain(notes[0]!.text);
  expect(nodes.filter(x => x.type === "p").map(x => x.props.children)).toContain(notes[2]!.text);
});

it("renders full campaign notes after plan and next, with owner labels and an older-hub empty state", () => {
  const data: CampaignView = {
    root: { id: 1, name: "release", role: "orchestrator", status: "open", host: "", created_at: "", closed_at: "", next: "Review" },
    goal: null, plan: null, documents: [], decisions: [], handovers: [], lanes: [], page: 1, pages: 1, total: 0,
    notes: [{ ...notes[2]!, owner: false }, { ...notes[0]!, owner: true }],
  };
  const nodes = children(Campaign({ data, changePage: () => {}, revision: 0 }));
  expect(nodes.map(x => x.props.title).filter(Boolean).slice(0, 4)).toEqual(["Goal", "Plan", "Next step", "Notes"]);
  const section = nodes.find(x => x.props.title === "Notes")!;
  const noteNodes = children(section);
  expect(noteNodes.filter(x => x.type === "time").map(x => x.props.dateTime)).toEqual([notes[0]!.at, notes[2]!.at]);
  expect(noteNodes.filter(x => x.type === "time").map(x => x.props.children)).toEqual([noteTime(notes[0]!.at), noteTime(notes[2]!.at)]);
  expect(noteNodes.filter(x => x.props.class === "ledger-text").map(x => x.props.children)).toEqual([notes[0]!.text, notes[2]!.text]);
  expect(noteNodes.filter(x => x.props.class === "owner-note-label")).toHaveLength(1);
  delete data.notes;
  expect(children(Campaign({ data, changePage: () => {}, revision: 0 })).some(x => x.props.children === "No notes recorded.")).toBe(true);
});
