import type { ComponentChildren } from "preact";
import type { AttentionItem, PlanRef } from "../api";
import { ago, noteTime, cueAction, cueLabel, signalWord, umdCounts } from "../model";
import { campaignItem, type WorkItem } from "../work";
import { Mark } from "./Mark";

export function Detail({ item, now, open }: { item: WorkItem; now: number; open: (item: WorkItem) => void }) {
  const d = item.detail;
  return (
    <>
      <div class="detail-title">
        <span class={"status " + item.tone}><Mark icon={item.icon} />{item.status}</span>
        <h2>{item.title}</h2>
        <p class="detail-context">{[item.context, item.machine].filter(Boolean).join(" · ")}</p>
        {item.at && <p class="muted">Updated {ago(item.at, now)}</p>}
      </div>
      {d.kind === "cue" && <CueDetail cue={d.cue} />}
      {d.kind === "owner-notes" && <Section title="For you"><ul class="full-list">{d.notes.map(n => <li key={n.id}><time class="history-meta" dateTime={n.at}>{noteTime(n.at)} · {ago(n.at, now)}</time><p>{n.text}</p></li>)}</ul><a class="text-button" href={"#/campaign/" + d.notes[0]!.root_id}>Campaign page</a></Section>}
      {d.kind === "milestone" && <>
        <Section title="Checkpoint">{d.milestone.text || "No checkpoint text recorded."}</Section>
        <Properties values={[["Campaign", d.milestone.orchestrator], ["Lane", d.milestone.lane], ["Role", d.milestone.role], ["Event", String(d.milestone.id)]]} />
      </>}
      {d.kind === "campaign" && <>
        {(d.machine.stale || (d.machine.state.daemon && d.machine.state.daemon.state !== "fresh")) && <p class="notice">Machine data may be out of date. Check the machine or daemon before acting.</p>}
        <Properties values={[["Machine identity", d.machine.node_id ?? "local"], ["Task", String(d.lane?.id ?? d.campaign.id)], ["Role", d.lane?.role ?? d.campaign.role], ["Ledger status", d.lane?.status ?? d.campaign.status], ["Agent", d.lane?.agent_status], ["Pane", d.lane?.pane_id ?? d.campaign.pane_id], ["Open asks", String(d.lane?.open_asks ?? d.campaign.open_asks)]]} />
        <Section title="Next step">{d.lane ? d.lane.next?.text ?? "No next step recorded." : d.campaign.next?.text ?? "No next step recorded."}</Section>
        {d.lane ? <>
          <Section title="Latest update">{d.lane.last_event?.summary ?? "No update text recorded."}</Section>
          {d.lane.report_path && <Section title="Report"><code>{d.lane.report_path}</code></Section>}
          <Refs refs={d.lane.refs} />
          <button class="text-button" onClick={() => open(campaignItem(d.machine, d.campaign))}><Mark icon="back" />Open campaign</button>
        </> : <>
          {d.machine.local && <a class="text-button" href={"#/campaign/" + d.campaign.id}>Campaign page</a>}
          <Section title="Latest update">{d.campaign.note?.text ?? "No status note recorded."}</Section>
          <Section title="Lanes">
            <p class="muted">{umdCounts(d.campaign)}</p>
            {!!d.campaign.tasks_truncated && <p class="notice">{d.campaign.tasks_truncated} additional lanes omitted by the server. This list is incomplete.</p>}
            <ul class="detail-lanes">{d.campaign.tasks.map(t => {
              const x = campaignItem(d.machine, d.campaign, t);
              return <li key={x.key}><button onClick={() => open(x)}><Mark icon={x.icon} /><span>{t.name}</span><span class={"status " + x.tone}>{x.status}</span></button></li>;
            })}</ul>
            {!d.campaign.tasks.length && <p class="muted">No open lanes in this snapshot.</p>}
          </Section>
          <Refs refs={d.campaign.refs} />
          {!!d.campaign.unread_work?.length && <Section title="Work waiting"><ul class="full-list">{d.campaign.unread_work.map(w => <li key={w.event_id}><b>{w.from.name} · {w.kind}</b><p>{w.summary}</p><p class="muted">{w.count} event(s) · waiting on {w.recipient.name}</p></li>)}</ul></Section>}
          {!!d.campaign.decisions?.length && <details class="disclosure"><summary>Decisions <span>{d.campaign.decisions.length}</span></summary><ul class="full-list">{d.campaign.decisions.map(x => <li key={x.id}><p>{x.text}</p>{x.answer && <p><b>Answer: </b>{x.answer}</p>}</li>)}</ul></details>}
          {d.campaign.last_handover && <details class="disclosure"><summary>Latest handover</summary><p class="full-text">{d.campaign.last_handover.text}</p></details>}
        </>}
      </>}
    </>
  );
}

function CueDetail({ cue: c }: { cue: AttentionItem }) {
  return <>
    <Section title={c.kind === "owner_ask" ? "Owner question" : "Attention signal"}>{c.text}</Section>
    <Section title="Where to act"><b>{cueAction(c)}</b><p>{c.action}</p><p class="muted">Act in the specified pane or on {c.machine}. This dashboard is read-only.</p></Section>
    <Properties values={[["Pane", c.pane_id], ["Machine identity", c.node_id ?? (c.local ? "local" : c.machine)], ["Ask", c.ask_id === undefined ? undefined : String(c.ask_id)], ["Blocking", c.blocking ? "Yes" : undefined], ["Asker waiting", c.asker_waiting ? "Yes" : undefined]]} />
    {c.command && <Section title="Command"><code>{c.command}</code></Section>}
    {c.report && <Section title="Report"><code>{c.report}</code></Section>}
    {!!c.also?.length && <Section title="Lane also needs attention">{c.also.map((x, i) => <div class="folded-signal" key={i}><h4>{cueLabel(x.kind)}</h4><p>{x.text}</p><p>{x.action}</p>{x.command && <code>{x.command}</code>}{x.report && <code>{x.report}</code>}</div>)}</Section>}
    {!!c.also?.length && <p class="muted">{c.also.map(x => signalWord(x.kind)).join(" · ")} signals are included in this ask, not counted as separate inbox rows.</p>}
  </>;
}

function Section({ title, children }: { title: string; children: ComponentChildren }) {
  return <section class="detail-section"><h3>{title}</h3><div class="full-text">{children}</div></section>;
}
function Properties({ values }: { values: [string, string | undefined][] }) {
  return <dl class="properties">{values.filter(([, v]) => v !== undefined && v !== "").map(([k, v]) => <div key={k}><dt>{k}</dt><dd>{v}</dd></div>)}</dl>;
}
function Refs({ refs }: { refs?: PlanRef[] }) {
  return refs?.length ? <Section title="References"><dl class="properties">{refs.map(r => <div key={r.key}><dt>{r.key}</dt><dd>{r.value}</dd></div>)}</dl></Section> : null;
}
