---
version: 1
slug: "dashboard-html"
primary_target: "web/src/App.tsx"
related_targets: ["web/src/styles.css", "web/src/components/Dashboard.tsx", "web/src/components/Detail.tsx", "web/src/components/Mark.tsx", "web/src/work.ts"]
---
# Surface: taskr owner dashboard

Mode: Operate. A pinned browser tab beside Herdr, scanned between tasks; phone is the same product with a focused detail page. Read-only ledger; answer and act in the named orchestrator pane. Preserve the API and polling contract.

## Direction contract

THESIS: Linear's familiar priority inbox and compact work lists, explicitly requested by the owner, replace the expanded all-prose dashboard.

OWN-WORLD: System-ui, 13px rows, 12px metadata, 15px view headings. Neutral light/dark OS surfaces, tonal sidebar/group headers, thin rules, small consistent SVG status circles, one subdued violet selection accent. Status always has words.

STORY: Read the global attention count; scan owner asks, interruptions and waiting work; open one row for its complete text and actual destination. Switch to campaigns to see running lanes and next steps, or Activity for checkpoints and raw history. Machine filters never change global counts.

FIRST VIEWPORT: A 216px navigation rail and compact content header; Inbox is the initial view. Filter controls sit above 58px attention rows. Global counts remain visible, with compact campaign summaries beneath the inbox. No expanded ledger paragraphs. Selecting a row opens a 440px detail pane on wide screens; phone replaces the list with a back-linked detail view.

FORM: Owner-pinned Linear category standard, code-led implementation, no concept seed required. Signature interaction is row-to-focused-detail with keyboard focus and restoration; no decorative entrance motion or mutation controls.

FINISH: Complete research, synthetic desktop/phone light/dark and quiet/busy browser evidence, source/type/tests/build and one scoped detector pass. Independent review is owned by the parent campaign; this implementer must not delegate. Document the shipped system in docs/DESIGN.md and .impeccable/design.json; all QA evidence stays in the authorized report directory.
