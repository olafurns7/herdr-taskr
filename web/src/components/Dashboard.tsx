import type { ComponentChildren } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import type { StateView } from "../api";
import { ageOf, ago, plural, programCounts, short, signalWord, type Icon } from "../model";
import { campaignItem, filterItems, inboxCount, inboxScope, machineKey, workItems, type InboxScope, type WorkItem } from "../work";
import { Detail } from "./Detail";
import { Mark } from "./Mark";

type View = "Inbox" | "Campaigns" | "Activity";
const NAV: [View, Icon][] = [["Inbox", "inbox"], ["Campaigns", "campaign"], ["Activity", "activity"]];

export function Dashboard({ state: s, now, lost, readThrough, markRead }: { state: StateView | null; now: number; lost: number | null; readThrough: number; markRead: () => void }) {
  const [view, setView] = useState<View>("Inbox");
  const [machine, setMachine] = useState("");
  const [query, setQuery] = useState("");
  const [scope, setScope] = useState<InboxScope>("all");
  const [selected, setSelected] = useState<string | null>(null);
  const origin = useRef<HTMLElement | null>(null);
  const detail = useRef<HTMLElement>(null);
  const main = useRef<HTMLElement>(null);
  const data = s ? workItems(s, readThrough) : { inbox: [], ownerNotes: [], campaigns: [], lanes: [], milestones: [] };
  const counts = programCounts(s?.attention ?? []);
  const all = [...data.inbox, ...data.ownerNotes, ...data.campaigns, ...data.lanes, ...data.milestones];
  const current = all.find(x => x.key === selected);
  const filtered = (items: WorkItem[]) => filterItems(items, machine, query);
  const totalInbox = inboxCount(data);
  const scoped = inboxScope(data, scope);
  const inbox = filtered(scoped.inbox);
  const ownerNotes = filtered(scoped.ownerNotes);
  const campaigns = filtered(data.campaigns);
  const milestones = filtered(data.milestones);
  const open = (x: WorkItem, element?: HTMLElement) => {
    if (element) origin.current = element;
    setSelected(x.key);
  };
  const close = () => {
    setSelected(null);
    requestAnimationFrame(() => (origin.current?.isConnected ? origin.current : main.current)?.focus());
  };
  useEffect(() => {
    if (selected) {
      detail.current?.focus({ preventScroll: true });
      detail.current?.scrollTo(0, 0);
      if (window.matchMedia("(max-width: 1200px)").matches) window.scrollTo(0, 0);
    }
  }, [selected]);
  const navigate = (next: View) => {
    setView(next); setSelected(null); setQuery(""); setScope("all");
  };
  const stale = s?.machines.filter(m => m.stale || (m.state.daemon && m.state.daemon.state !== "fresh")).length ?? 0;
  const connection = !s ? lost === null ? "Connecting" : "Offline" : lost !== null ? "Offline · saved snapshot" : stale ? plural(stale, "machine needs checking", "machines need checking") : "Connected";

  return (
    <div class="app-shell">
      <a class="skip-link" href="#main">Skip to work</a>
      <aside class="sidebar" aria-label="Workspace">
        <div class="workspace"><span class="workspace-mark"><Mark icon="campaign" /></span><span class="brand">taskr</span><span class="workspace-meta">{s?.hub ? "Hub" : "Workspace"}</span></div>
        <nav class="primary-nav" aria-label="Dashboard">
          {NAV.map(([name, icon]) => <button key={name} class={view === name ? "nav-item active" : "nav-item"} aria-current={view === name ? "page" : undefined} onClick={() => navigate(name)}><Mark icon={icon} /><span>{name}</span><span class={"nav-count" + (name === "Inbox" && counts.red ? " urgent" : "")}>{s ? name === "Inbox" ? totalInbox : name === "Campaigns" ? data.campaigns.length : data.milestones.length : "—"}</span></button>)}
        </nav>
        <div class="machine-nav">
          <h2>Machines</h2>
          <button class={"nav-item" + (!machine ? " active" : "")} aria-pressed={!machine} onClick={() => { setMachine(""); setSelected(null); }}><Mark icon="machine" /><span>All machines</span><span class="nav-count">{s?.machines.length ?? "—"}</span></button>
          {s?.machines.map(m => {
            const health = m.stale ? "Stale" : m.state.daemon && m.state.daemon.state !== "fresh" ? "Daemon " + m.state.daemon.state : m.local ? "Local" : "Synced";
            return <button key={machineKey(m)} class={"nav-item machine-item" + (machine === machineKey(m) ? " active" : "")} aria-pressed={machine === machineKey(m)} onClick={() => { setMachine(machineKey(m)); setSelected(null); }} title={m.machine + " · " + (m.node_id ?? "local")}><Mark icon="machine" /><span class="machine-name">{m.machine}</span><span class={"machine-health" + (health === "Stale" || health.startsWith("Daemon") ? " amber" : "")}>{health}</span></button>;
          })}
        </div>
        <footer class="sidebar-footer"><span><Mark icon="open" />Read-only workspace</span><p>Act in the orchestrator's pane.</p>{s?.version && <p>taskr {s.version}</p>}</footer>
      </aside>
      <main id="main" class="main" ref={main} tabIndex={-1}>
        <header class="view-header"><h1><Mark icon={NAV.find(([name]) => name === view)![1]} />{view}</h1><a class="text-button" href="#/campaigns">All campaigns</a><span class={"connection" + (lost !== null || stale ? " amber" : "")}><span class="connection-dot" aria-hidden="true" />{connection}</span></header>
        <div class="global-summary" role="status" aria-live="polite">
          {s ? <><span class={counts.red ? "red" : "muted"}>{counts.red} need attention</span><span class={counts.amber ? "amber" : "muted"}>{counts.amber} work waiting</span><span class="summary-running">{data.lanes.filter(x => x.status === "working").length} working</span><span class="summary-update">{lost === null ? "Updated just now" : "Last updated " + ago(new Date(lost || Date.now()).toISOString(), Date.now())}</span></> : <span>Waiting for the ledger snapshot</span>}
        </div>
        {lost !== null && <div class="notice offline" role="status">{lost ? "Cannot reach the taskr daemon. This saved snapshot may be out of date." : "Cannot reach the taskr daemon. Check that it is running: taskr daemon --status"}</div>}
        <div class={"work-area" + (selected ? " has-detail" : "")}>
          <div class="list-pane">
            <div class="toolbar">
              {view === "Inbox" ? <div class="scope-buttons" aria-label="Inbox filters">{([["all", "All"], ["attention", "Needs attention"], ["waiting", "Waiting"]] as const).map(([value, label]) => <button key={value} aria-pressed={scope === value} class={scope === value ? "selected" : ""} onClick={() => setScope(value)}>{label}<span>{s ? value === "all" ? totalInbox : value === "attention" ? counts.red : counts.amber : "—"}</span></button>)}</div> : <p class="toolbar-label">{view === "Campaigns" ? "Campaigns and lanes" : "Recent checkpoints"}</p>}
              <label class="search"><Mark icon="search" /><input type="search" aria-label={"Search " + view.toLowerCase()} placeholder="Search…" value={query} onInput={e => setQuery(e.currentTarget.value)} /></label>
              <label class="mobile-machine"><span class="sr-only">Filter by machine</span><select value={machine} onChange={e => { setMachine(e.currentTarget.value); setSelected(null); }}><option value="">All machines</option>{s?.machines.map(m => <option key={machineKey(m)} value={machineKey(m)}>{m.machine}</option>)}</select></label>
            </div>
            {(machine || query || (view === "Inbox" && scope !== "all")) && <div class="filter-notice"><span>Filtered view · global attention counts above include all machines</span><button class="text-button" onClick={() => { setMachine(""); setQuery(""); setScope("all"); }}>Clear filters</button></div>}
            {!s ? <div class="loading" role="status"><h2>{lost === null ? "Connecting to taskr" : "No snapshot available"}</h2><p>The inbox and campaigns appear when the daemon responds.</p>{lost === null && <div class="skeleton" aria-hidden="true"><div /><div /><div /></div>}</div> : <>
              {view === "Inbox" && <>
                <InboxGroups inbox={inbox} ownerNotes={ownerNotes} total={totalInbox} unavailable={lost !== null || !!stale} selected={selected} now={now} open={open} markRead={markRead} />
                <Group title="Campaigns" count={campaigns.length} action={<button class="text-button" onClick={() => navigate("Campaigns")}>View lanes<Mark icon="open" /></button>}>{campaigns.length ? campaigns.map(x => <Row key={x.key} item={x} selected={selected} now={now} open={open} />) : <li class="empty-inline">{machine || query ? "No matching campaigns." : "No open campaigns in this snapshot."}</li>}</Group>
              </>}
              {view === "Campaigns" && <>
                {s.machines.flatMap(m => m.state.orchestrators.map(o => {
                  const root = campaignItem(m, o);
                  const lanes = filtered(o.tasks.map(t => campaignItem(m, o, t)));
                  const rootMatches = filtered([root]).length > 0;
                  if (!rootMatches && !lanes.length) return null;
                  return <Group key={root.key} title={o.name} count={lanes.length} subtitle={m.machine} action={m.local ? <a class="text-button" href={"#/campaign/" + o.id}>Campaign page</a> : undefined}>
                    <Row item={root} selected={selected} now={now} open={open} />
                    {lanes.map(x => <Row key={x.key} item={x} selected={selected} now={now} open={open} lane />)}
                    {!!o.tasks_truncated && <li class="truncation">{o.tasks_truncated} additional lanes omitted by the server. Open the campaign for details.</li>}
                  </Group>;
                }))}
                {!filtered([...data.campaigns, ...data.lanes]).length && <Empty title="No matching campaigns" text={machine || query ? "Adjust the filters to see the other work." : "No campaigns are open in this snapshot."} />}
              </>}
              {view === "Activity" && <>
                <Group title="Checkpoints" count={milestones.length}>{milestones.length ? milestones.map(x => <Row key={x.key} item={x} selected={selected} now={now} open={open} />) : <li class="empty-inline">No matching checkpoints.</li>}</Group>
                <History state={s} machine={machine} query={query} now={now} />
              </>}
            </>}
          </div>
          {selected && <section id="detail-view" class="detail-pane" aria-label="Work details" ref={detail} tabIndex={-1} onKeyDown={e => { if (e.key === "Escape") { e.stopPropagation(); close(); } }}>
            <div class="detail-bar"><button class="text-button" onClick={close}><Mark icon="back" />Back to {view.toLowerCase()}</button><span>Read-only</span></div>
            {current ? <Detail item={current} now={now} open={open} /> : <Empty title="Item no longer in this snapshot" text="It may have been resolved or omitted by the server. Return to the list for current work." />}
          </section>}
        </div>
      </main>
    </div>
  );
}

export function InboxGroups({ inbox, ownerNotes, total, unavailable, selected, now, open, markRead }: { inbox: WorkItem[]; ownerNotes: WorkItem[]; total: number; unavailable: boolean; selected: string | null; now: number; open: (x: WorkItem, el?: HTMLElement) => void; markRead: () => void }) {
  const groups: [string, WorkItem[]][] = [
    ["Owner asks", inbox.filter(x => x.detail.kind === "cue" && x.detail.cue.kind === "owner_ask")],
    ["For you", ownerNotes],
    ["Needs checking", inbox.filter(x => x.detail.kind === "cue" && x.detail.cue.kind !== "owner_ask" && x.tone === "red")],
    ["Work waiting", inbox.filter(x => x.tone === "amber")],
  ];
  return inbox.length || ownerNotes.length ? <>{groups.map(([name, items]) => items.length ? <Group key={name} title={name} count={items.length} action={name === "For you" && items.some(x => x.isNew) ? <button class="text-button" onClick={markRead}>Mark read</button> : undefined}>{items.map(x => <Row key={x.key} item={x} selected={selected} now={now} open={open} />)}</Group> : null)}</> : <Empty title={total ? "No matching inbox items" : unavailable ? "No inbox items in this snapshot" : "You're up to date"} text={total ? "Adjust the filters to see the other inbox items. Global counts remain above." : unavailable ? "Machine data may be incomplete or out of date. Check connection health before relying on this snapshot." : "Nothing currently needs your attention. Campaign progress is below."} />;
}

function Group({ title, count, subtitle, action, children }: { title: string; count: number; subtitle?: string; action?: ComponentChildren; children: ComponentChildren }) {
  return <section class="work-group" aria-label={title + (subtitle ? " · " + subtitle : "")}><header class="group-header"><h2>{title}<span>{count}</span></h2>{subtitle && <span class="group-machine">{subtitle}</span>}{action}</header><ul class="work-list">{children}</ul></section>;
}
function Row({ item: x, selected, now, open, lane = false }: { item: WorkItem; selected: string | null; now: number; open: (x: WorkItem, el?: HTMLElement) => void; lane?: boolean }) {
  const cue = x.detail.kind === "cue" ? x.detail.cue : null;
  const campaign = x.detail.kind === "campaign" && !x.detail.lane ? x.detail.campaign : null;
  const tags = cue ? [...(cue.blocking ? ["blocking"] : []), ...(cue.also ?? []).map(s => signalWord(s.kind))] : [];
  return <li><button class={"work-row " + x.tone + (x.detail.kind === "owner-notes" && !x.isNew ? " row-read" : "") + (lane ? " lane-row" : "") + (selected === x.key ? " row-selected" : "")} aria-expanded={selected === x.key} aria-controls="detail-view" onClick={e => open(x, e.currentTarget)}>
    <span class="row-icon"><Mark icon={x.icon} /></span>
    <span class="row-content"><span class="row-title">{x.title}</span><span class="row-preview">{campaign?.next ? <><span class="next-label">Next</span> {x.preview}</> : x.preview || x.context || "Open details"}</span></span>
    <span class="row-meta"><span class="row-status">{x.status}{tags.length > 0 && <span class="folded-tags"> · {tags.join(" · ")}</span>}</span><span class="row-context">{[x.machine, x.context].filter(Boolean).join(" · ")}</span></span>
    <span class="row-age" title={x.at ? ago(x.at, now) : undefined}>{x.at ? short(ageOf(x.at, now)) : ""}</span>
  </button>{campaign && x.detail.kind === "campaign" && x.detail.machine.local && <a class="campaign-page-link" href={"#/campaign/" + campaign.id}>Campaign page</a>}</li>;
}
function Empty({ title, text }: { title: string; text: string }) {
  return <div class="empty"><Mark icon="inbox" /><h2>{title}</h2><p>{text}</p></div>;
}
function History({ state: s, machine, query, now }: { state: StateView; machine: string; query: string; now: number }) {
  const machines = s.machines.filter(m => !machine || machineKey(m) === machine);
  const matches = (text: string) => query.toLowerCase().trim().split(/\s+/).every(w => text.toLowerCase().includes(w));
  const activity = machines.flatMap(m => m.state.activity.map(e => ({ m, e }))).filter(({ m, e }) => matches([m.machine, e.root_name, e.task.name, e.kind, e.summary].join(" "))).sort((a, b) => Date.parse(b.e.at) - Date.parse(a.e.at));
  const rules = machines.flatMap(m => m.state.orchestrators.flatMap(o => (o.decisions ?? []).map(d => ({ m, o, d })))).filter(({ m, o, d }) => matches([m.machine, o.name, d.text, d.answer].join(" ")));
  const closed = machines.flatMap(m => m.state.closed.map(c => ({ m, c }))).filter(({ m, c }) => matches([m.machine, c.name, c.note?.text].join(" ")));
  return <div class="history">
    <details class="disclosure"><summary>Raw activity <span>{activity.length}</span></summary><ul class="full-list">{activity.map(({ m, e }) => <li key={machineKey(m) + ":" + e.id}><b>{e.kind} · {e.task.name}</b><span class="history-meta">{m.machine} · {e.root_name} · {ago(e.at, now)}</span><p>{e.summary}</p></li>)}</ul>{!activity.length && <p class="muted">No matching activity.</p>}</details>
    <details class="disclosure"><summary>Decisions <span>{rules.length}</span></summary><ul class="full-list">{rules.map(({ m, o, d }) => <li key={machineKey(m) + ":" + o.id + ":" + d.id}><b>{o.name}</b><span class="history-meta">{m.machine}</span><p>{d.text}</p>{d.answer && <p><b>Answer: </b>{d.answer}</p>}</li>)}</ul>{!rules.length && <p class="muted">No matching decisions.</p>}</details>
    <details class="disclosure"><summary>Closed in the last day <span>{closed.length}</span></summary><ul class="full-list">{closed.map(({ m, c }) => <li key={machineKey(m) + ":" + c.id}><b>{c.name}</b><span class="history-meta">{m.machine} · {ago(c.closed_at, now)}</span><p>{c.note?.text}</p></li>)}</ul>{!closed.length && <p class="muted">No matching closed campaigns.</p>}</details>
    <details class="disclosure"><summary>Connection details <span>{machines.length}</span></summary><ul class="full-list">{machines.map(m => <li key={machineKey(m)}><b>{m.machine}</b><p>Identity: {m.node_id ?? "local"} · {m.stale ? "Stale" : "Snapshot received"}</p><p>{m.local ? "Local ledger" : "Last heard " + short(m.last_push_age_ms) + " ago"} · Daemon: {m.state.daemon?.state ?? "Not reported"}</p></li>)}</ul></details>
  </div>;
}
