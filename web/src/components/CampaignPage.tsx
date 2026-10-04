import type { ComponentChildren } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import { fetchCampaignData, type CampaignArchive, type CampaignView, type DocumentMeta, type DocumentView, type PageInfo } from "../campaign-api";
import type { Route } from "../router";
import { noteTime } from "../model";
import { Mark } from "./Mark";
import { MarkdownBoundary, PlainDocument } from "./DocumentBody";

type MarkdownComponent = typeof import("../markdown").Markdown;

function useLoaded<T>(path: string, revision: number) {
  const [result, setResult] = useState<{ data?: T; error?: string }>({});
  useEffect(() => {
    const abort = new AbortController();
    setResult({});
    fetchCampaignData<T>(path, abort.signal).then(
      data => { if (!abort.signal.aborted) setResult({ data }); },
      error => { if (!abort.signal.aborted) setResult({ error: String(error.message ?? error) }); },
    );
    return () => abort.abort();
  }, [path, revision]);
  return result;
}

export function CampaignPage({ route }: { route: Exclude<Route, { view: "dashboard" }> }) {
  const [page, setPage] = useState(1);
  const [revision, setRevision] = useState(0);
  const path = route.view === "campaigns" ? "/api/campaigns?page=" + page : "/api/" + route.view + "/" + route.id + "?page=" + page;
  const { data, error } = useLoaded<CampaignArchive | CampaignView | DocumentView>(path, revision);
  const main = useRef<HTMLElement>(null);
  useEffect(() => { main.current?.focus({ preventScroll: true }); window.scrollTo(0, 0); }, []);
  const title = route.view === "campaigns" ? "All campaigns" : route.view === "campaign" ? (data as CampaignView | undefined)?.root?.name ?? "Campaign" : "Document";
  useEffect(() => { document.title = title + " · taskr"; }, [title]);
  return <main class="ledger-page" ref={main} tabIndex={-1}>
    <header class="view-header"><a class="text-button" href="#/"><Mark icon="back" />Dashboard</a><h1>{title}</h1><button class="text-button" onClick={() => setRevision(x => x + 1)}>Reload</button></header>
    <div class="ledger-content">
      {route.view !== "campaigns" && <nav class="ledger-nav" aria-label="Ledger"><a href="#/campaigns">All campaigns</a>{route.view === "doc" && data && <a href={"#/campaign/" + (data as DocumentView).root_id}>Campaign</a>}</nav>}
      {error ? <p class="notice" role="alert">{error} Use Reload to try again.</p> : !data ? <p class="muted" role="status">Loading {route.view === "campaigns" ? "campaigns" : route.view}…</p> : route.view === "campaigns" ? <Archive data={data as CampaignArchive} changePage={setPage} /> : route.view === "campaign" ? <Campaign data={data as CampaignView} changePage={setPage} revision={revision} /> : <Document data={data as DocumentView} />}
    </div>
  </main>;
}
function Status({ status }: { status: string }) {
  const icon = status === "closed" || status === "done" ? "done" : status === "failed" ? "failed" : status === "ready" ? "ready" : status === "planned" ? "planned" : "working";
  return <span class={"status " + (icon === "failed" ? "red" : icon === "done" || icon === "ready" ? "green" : "muted")}><Mark icon={icon} />{status}</span>;
}
function Times({ created, closed }: { created: string; closed: string }) {
  return <p class="ledger-meta">Created <time dateTime={created}>{created}</time>{closed && <> · Closed <time dateTime={closed}>{closed}</time></>}</p>;
}
function Paging({ info, changePage, previous, next }: { info: PageInfo; changePage: (n: number) => void; previous: string; next: string }) {
  return <nav class="ledger-paging" aria-label="Pages"><button class="text-button" disabled={info.page <= 1} onClick={() => changePage(info.page - 1)}>{previous}</button><span>Page {info.page} of {info.pages} · {info.total} total</span><button class="text-button" disabled={info.page >= info.pages} onClick={() => changePage(info.page + 1)}>{next}</button></nav>;
}
function Archive({ data, changePage }: { data: CampaignArchive; changePage: (n: number) => void }) {
  return <><ul class="archive-list">{data.campaigns.map(c => <li key={c.id}><div class="archive-heading"><a href={"#/campaign/" + c.id}>{c.name}</a><Status status={c.status} /></div><Times created={c.created_at} closed={c.closed_at} />{c.goal && <p class="archive-goal">{c.goal}</p>}<p class="ledger-meta">{Object.entries(c.lane_counts).map(([status, count]) => count + " " + status).join(" · ") || "No lanes"}</p></li>)}</ul>{!data.campaigns.length && <p class="muted">No campaigns recorded.</p>}<Paging info={data} changePage={changePage} previous="Newer" next="Older" /></>;
}
function Section({ title, children }: { title: string; children: ComponentChildren }) {
  return <section class="ledger-section"><h2>{title}</h2>{children}</section>;
}
function Miss({ doc }: { doc: DocumentMeta }) {
  return <p class="document-miss">Not captured: {doc.reason ?? "reason not recorded"} · <code>{doc.source_path ?? "No source path"}</code> · {doc.source_host ?? "server host"}</p>;
}
function DocLink({ doc, label }: { doc: DocumentMeta | null; label: string }) {
  return doc ? doc.captured ? <a href={"#/doc/" + doc.doc_id}>{label}</a> : <span>{label}: {doc.reason ?? "not captured"}</span> : <span class="muted">No {label}</span>;
}
function Captured({ doc, revision }: { doc: DocumentMeta; revision: number }) {
  const { data, error } = useLoaded<DocumentView>("/api/doc/" + doc.doc_id, revision);
  return <>{error ? <p class="notice" role="alert">{error}</p> : data ? <Body doc={data} /> : <p class="muted" role="status">Loading document…</p>}<p class="ledger-meta"><a href={"#/doc/" + doc.doc_id}>Document details · v{doc.version}</a></p></>;
}
export function Campaign({ data: c, changePage, revision }: { data: CampaignView; changePage: (n: number) => void; revision: number }) {
  const parents = new Map(c.lanes.map(l => [l.id, l.name]));
  return <>
    <Status status={c.root.status} /><Times created={c.root.created_at} closed={c.root.closed_at} />
    <Section title="Goal">{!c.goal ? <><p>No goal recorded</p><code>taskr doc set {c.root.id} goal --file PATH</code></> : c.goal.captured ? <Captured key={c.goal.doc_id} doc={c.goal} revision={revision} /> : <Miss doc={c.goal} />}{c.goal_miss && <p class="document-miss">A later version was not captured ({c.goal_miss.reason})</p>}</Section>
    <Section title="Plan">{c.plan ? <><p class="ledger-meta">Plan v{c.plan.version}: since then {c.plan.decisions_since} decisions, {c.plan.closed_since} lanes closed</p>{c.plan.captured ? <Captured key={c.plan.doc_id} doc={c.plan} revision={revision} /> : <Miss doc={c.plan} />}</> : <p class="muted">No plan recorded.</p>}</Section>
    {c.root.next && <Section title="Next step"><p class="ledger-text">{c.root.next}</p></Section>}
    <Section title="Notes"><ul class="ledger-notes">{[...(c.notes ?? [])].sort((a, b) => b.id - a.id).map(n => <li key={n.id}><p class="ledger-meta">{n.owner && <span class="owner-note-label">For you · </span>}<time dateTime={n.at}>{noteTime(n.at)}</time></p><p class="ledger-text">{n.text}</p></li>)}</ul>{!c.notes?.length && <p class="muted">No notes recorded.</p>}</Section>
    <Section title="Documents"><ul class="ledger-notes">{c.documents.map(doc => <li key={doc.doc_id}><a href={"#/doc/" + doc.doc_id}>{doc.name} · v{doc.version}</a>{!doc.captured && <Miss doc={doc} />}</li>)}</ul>{!c.documents.length && <p class="muted">No named documents.</p>}</Section>
    <Section title="Decisions in force"><ul class="ledger-notes">{c.decisions.map(d => <li key={d.id}><p class="ledger-meta">Event {d.id} · <time dateTime={d.time}>{d.time}</time></p><p class="ledger-text">{d.text}</p></li>)}</ul>{!c.decisions.length && <p class="muted">No decisions in force.</p>}</Section>
    <Section title="Handovers"><ul class="ledger-notes">{c.handovers.map(h => <li key={h.id}><p class="ledger-meta">Event {h.id} · <time dateTime={h.time}>{h.time}</time></p><p class="ledger-text">{h.note || "No handover note."}</p>{h.doc_id !== null && <a href={"#/doc/" + h.doc_id}>Captured handover</a>}</li>)}</ul>{!c.handovers.length && <p class="muted">No handovers recorded.</p>}</Section>
    <Section title="Lanes"><div class="ledger-table-wrap" tabIndex={0} role="region" aria-label="Lane table"><table class="ledger-table"><thead><tr><th scope="col">Name</th><th scope="col">Role</th><th scope="col">Model and effort</th><th scope="col">Status</th><th scope="col">Final summary</th><th scope="col">Documents</th></tr></thead><tbody>{c.lanes.map(l => <tr key={l.id}><th scope="row"><span class="lane-name">{Array.from({ length: Math.min(20, l.depth - 1) }, (_, i) => <span class="lane-indent" key={i} aria-hidden="true" />)}{l.name}</span><span class="ledger-meta">Task {l.id}</span>{l.depth > 1 && <span class="ledger-meta">{parents.has(l.parent_id) ? "Under " + parents.get(l.parent_id) : "Under task " + l.parent_id}</span>}</th><td>{l.role}</td><td>{[l.provider, l.model, l.effort].filter(Boolean).join(" / ") || "No launch recorded"}</td><td><Status status={l.status} /></td><td class="ledger-text">{l.summary || "No final summary"}</td><td><div class="document-links"><DocLink doc={l.brief} label="brief" /><DocLink doc={l.report} label="report" /></div></td></tr>)}</tbody></table></div>{!c.lanes.length && <p class="muted">No lanes recorded.</p>}<Paging info={c} changePage={changePage} previous="Previous" next="Next" /></Section>
  </>;
}
function Document({ data: d }: { data: DocumentView }) {
  return <><h2 class="document-title">{d.kind}{d.name && " / " + d.name} · {d.lane} · v{d.version}</h2><nav class="document-versions" aria-label="Document versions">{d.versions.map(v => <a key={v.id} href={"#/doc/" + v.id} aria-current={v.id === d.doc_id ? "page" : undefined}>v{v.version}</a>)}</nav><dl class="properties"><div><dt>Captured</dt><dd>{d.captured ? d.captured_at ?? d.created_at : "Not captured"}</dd></div><div><dt>Event</dt><dd>{d.event_id ?? "No event recorded"}</dd></div><div><dt>Source path</dt><dd><code>{d.source_path ?? "No source path"}</code></dd></div><div><dt>Source host</dt><dd>{d.source_host ?? "server host"}</dd></div></dl><Section title="Body"><Body doc={d} /></Section></>;
}
function Body({ doc }: { doc: DocumentView }) {
  const [Renderer, setRenderer] = useState<MarkdownComponent | null>(null);
  const [error, setError] = useState(false);
  useEffect(() => {
    if (!doc.captured || doc.format !== "md") return;
    let active = true;
    import("../markdown").then(module => { if (active) setRenderer(() => module.Markdown); }, () => { if (active) setError(true); });
    return () => { active = false; };
  }, [doc.doc_id, doc.captured, doc.format]);
  if (!doc.captured) return <Miss doc={doc} />;
  if (doc.format !== "md") return <pre class="document-text">{doc.body}</pre>;
  return error ? <PlainDocument body={doc.body ?? ""} /> : Renderer ? <MarkdownBoundary key={doc.doc_id} body={doc.body ?? ""}><Renderer body={doc.body ?? ""} /></MarkdownBoundary> : <p class="muted" role="status">Loading Markdown…</p>;
}
