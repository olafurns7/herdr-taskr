# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

The dashboard is a Vite + Preact + TypeScript app in `web/`, compiled to static files embedded in the Go binary and served by the taskr daemon. The Go side owns the ledger and `/api/state`. Node is needed for development and release builds, not on machines running taskr.

## Users

Developers coordinating several agent orchestrators and worker lanes, on one machine or across machines. They check the dashboard between tasks to see what needs attention, what reached a milestone, and where each campaign stands.

The dashboard supports scanning short rows on desktop and phone. Users open a focused detail view for complete ledger text, rather than reading every campaign and lane note at once.

## Product Purpose

taskr is a task ledger for Herdr orchestrators (one SQLite ledger on a server host, a CLI, and a daemon shipped as a Herdr plugin). Client hosts send ledger commands to the server. Its dashboard summarizes campaigns across those hosts. It helps users see open questions, meaningful activity, and campaign progress.

## Positioning

The dashboard reads the durable ledger that orchestrators and workers update as they work (registrations, prompt receipts, reports, asks and answers, decisions, next steps, references, and handovers). A Tailscale-authenticated server accepts client commands into one ledger; legacy peers appear as separate read-only snapshots, without ledger merging. The dashboard shows structured campaign state rather than logs or terminal output; users answer owner asks in the orchestrator's pane.

## Current architecture (v0.10)

One ledger on one server host. The server daemon (`dashboard.addr` = `tailnet`) serves the dashboard and `/api/rpc`; a client host (`~/.local/state/taskr/server.url`, no `TASKR_DB`) sends its ledger commands there without opening a local ledger; `help`, `version` and `daemon` run locally. Its daemon reports its own Herdr panes to the server and shows owner-ask notifications; its `prompt` command delivers prompts to its own lanes. Server unreachable: client records got/ready/done/fail/decide/next/note/close queue in the spool (`qd1`, exit 0; full or unwritable spool: exit 5 with a retry line); other commands fail with exit 5 and a retry line; nothing falls back to a local ledger. A host without `server.url` keeps its own ledger and pushes to the hub as a v0.5 peer when `hub.url` is configured, until it is moved over.

## Operating Context

- The daemon serves the dashboard on loopback by default; users can optionally configure a Tailscale hub to share state across machines.
- The page polls `/api/state` every few seconds; it has no push channel.
- Orchestrators and workers run in Herdr panes using each user's existing agent and model choices.
- Terminology: orchestrator (a root task), lane (a worker task under it), campaign (an orchestrator and its lanes), owner ask, ready/done/fail, next, decision, handover/adopt, ref, hub, and peer machine.

## Capabilities and Constraints

- Data available per machine: open orchestrators and their descendant lanes (status open/planned/ready/done/failed/closed, observed Herdr agent state and when, next step, refs, report path, and open asks), each orchestrator's latest note, decisions in force, last handover, the activity feed, recently closed campaigns, owner asks, per-machine staleness, and last push time. The server ledger also supplies owner notes from the last 48 hours on open campaigns and campaigns closed in the last day, up to ten per campaign, with text up to 4,000 runes.
- Needs attention first: open owner asks; blocked or failed lanes (Herdr sees a blocking dialog, a `fail` report, or the worker vanished); a stale machine or unhealthy daemon; an orchestrator that has not acknowledged a lane's ready/done for a while.
- For you: owner notes sit directly below Owner asks, one row per campaign showing its newest note's first line and age. Opening a row shows that note in full, then its earlier owner notes newest first with their times. The group has its own neutral count; Inbox and All count attention items plus one owner-note row per campaign, while notes do not increase red attention counts. "You're up to date" appears only when there are no attention items and no owner notes.
- Mark read appears when a row is new and stores the newest owner-note event ID in one browser's localStorage. Newer rows show New and count separately in the dashboard page title; read rows stay listed with muted text. The marker survives reloads, is separate in each browser, updates that browser's other dashboard tabs, and never writes to the ledger. If storage cannot be read, every owner-note row starts new; if it cannot be written, Mark read lasts until the page reloads. Either way the page still works.
- Major checkpoints: lane ready/done; review verdicts and PR/release refs; handovers, adoptions and decisions; orchestrator notes (phase changes).
- Everything else is the quiet feed.
- The dashboard is read-only: it shows what needs a person and the next action; owner asks are answered in the orchestrator's pane.
- Technical constraints: Preact + TypeScript built with Vite and embedded in the Go binary; no runtime CDN or remote assets; webfonts and other assets must be bundled in the binary and served by the daemon; strict CSP with self-only script, style, and font sources; ledger strings are shown as plain text; captured documents are rendered from Markdown into DOM nodes through an allow-list; nothing is rendered as HTML and no remote asset is fetched; light and dark follow the OS setting; support desktop and phone widths. `/api/state` may gain fields, and older hubs ignore unknown peer fields.

## Evidence on Hand

- Current page: `web/src/App.tsx`, `web/src/styles.css`, `web/src/work.ts`, and `web/src/components/{Dashboard,Detail,Mark}.tsx`; `dashboard.go` serves the embedded build and ledger API. Inbox groups Owner asks, For you, Needs checking and Work waiting; Campaigns groups orchestrators and lanes; Activity contains checkpoints and supporting history. Sidebar navigation, filters and short rows lead to complete details; phone layouts keep these three views and a detail page. Missing owner-note fields from older hubs are treated as empty lists.
- Use the synthetic `web/src/fixtures/state.json` generated by `TestWebStateFixture` for representative campaign, lane, ask, machine, and milestone evidence. Do not depend on a live host or campaign.

## Product Principles

1. What needs a person comes first and is unmistakable; when nothing does, the page says so plainly.
2. Milestones over noise: show the checkpoints of each campaign; keep the raw event stream available but quiet.
3. Compact and readable at a glance: aligned rows and machine/campaign grouping let users scan many lanes; explicit status words and marks carry state alongside restrained colour.
4. Truthful: every signal comes from the ledger or Herdr's observed state; nothing is inferred beyond what the data says, and staleness is shown honestly.
5. Read-only and calm: the page informs; actions happen where the agents are.

## Campaign archive and documents

The local ledger archive, complete campaign history, and captured document versions are reachable at `#/campaigns`, `#/campaign/ID`, and `#/doc/ID`; `#/` keeps the dashboard. Other views load once, with Reload and paged lists, while dashboard polling is paused. Peer snapshots have no campaign-page links. Campaigns retain the goal, plan, named documents, decisions, handovers and every lane, including closed work. Notes appear after the plan and next step, before lanes: up to 50 root notes newest first, full plain text with preserved line breaks and a small For you label on owner notes. Older hubs that omit notes show an empty section. Goal selection matches the handover, with a notice when a later version was not captured.

Captured Markdown becomes allow-listed DOM nodes; plain text stays preformatted, and rendering failures retain the complete body as plain text with a notice. A link is made only for an absolute `http://`, `https://` or `mailto:` target; HTTP targets require a host. Images remain alt text and a plain URL. A document that was not captured shows its path, host and reason. No document HTML is interpreted and no remote asset is fetched.
