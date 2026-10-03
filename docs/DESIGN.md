---
name: taskr
description: A priority inbox and compact work lists for Herdr campaigns.
colors:
  page: "#ffffff"
  surface: "#f7f7f8"
  hover: "#f0f0f2"
  ink: "#232329"
  secondary: "#51515d"
  muted: "#696975"
  rule: "#e7e7eb"
  red: "#b53646"
  amber: "#886215"
  green: "#28764f"
  accent: "#5956ba"
  selection: "#eeedf9"
  focus: "#5956ba"
  page-dark: "#18191d"
  surface-dark: "#141519"
  hover-dark: "#23242a"
  ink-dark: "#ececf0"
  secondary-dark: "#bcbcc7"
  muted-dark: "#a1a1af"
  rule-dark: "#2c2d34"
  red-dark: "#f0939f"
  amber-dark: "#d9b975"
  green-dark: "#81c3a0"
  accent-dark: "#b6b2ff"
  selection-dark: "#2b2a42"
  focus-dark: "#b6b2ff"
typography:
  view:
    fontFamily: "system-ui, -apple-system, \"Segoe UI\", sans-serif"
    fontSize: "15px"
    fontWeight: 600
    lineHeight: 1.5
  body:
    fontFamily: "system-ui, -apple-system, \"Segoe UI\", sans-serif"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.5
  row:
    fontFamily: "system-ui, -apple-system, \"Segoe UI\", sans-serif"
    fontSize: "13px"
    fontWeight: 550
    lineHeight: 1.5
  metadata:
    fontFamily: "system-ui, -apple-system, \"Segoe UI\", sans-serif"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: 1.5
  detail:
    fontFamily: "system-ui, -apple-system, \"Segoe UI\", sans-serif"
    fontSize: "19px"
    fontWeight: 600
    lineHeight: 1.35
rounded:
  control: "5px"
  workspace: "7px"
spacing:
  inline: "8px"
  gutter: "24px"
  phone-gutter: "16px"
  section: "18px"
components:
  nav-item:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.secondary}"
    rounded: "{rounded.control}"
    padding: "7px 9px"
  nav-item-active:
    backgroundColor: "{colors.hover}"
    textColor: "{colors.ink}"
  work-row:
    textColor: "{colors.ink}"
    padding: "11px 24px"
    typography: "{typography.row}"
  work-row-selected:
    backgroundColor: "{colors.selection}"
  search-input:
    backgroundColor: "{colors.page}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: "5px 7px"
  group-header:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    padding: "7px 24px"
  detail-pane:
    backgroundColor: "{colors.page}"
    textColor: "{colors.secondary}"
    width: "440px"
---

# Design System: taskr

## Overview

**Creative North Star: "Focused work"**

Linear-inspired task management: compact grouped work, familiar navigation, and one focused detail view. The interface favors the owner's scanning task over expanded ledger prose.

Neutral OS themes fit a pinned browser tab used alongside Herdr in changing ambient light. Complete ledger content remains readable after opening a row; all controls change local presentation only.

**Key Characteristics:**
- Priority inbox and consistent list rows.
- Tonal navigation and group surfaces with thin dividers.
- One system font family and tabular numerals.
- Machine identity and global attention counts across filtered views.
- Focused details preserve complete text and actual destinations.

## Current architecture (v0.10)

The hub page reads one ledger: every client host's tasks live on the server, tagged with their machine. A lane whose host daemon has not reported for 30 s shows agent state `unknown`, not `missing`. A host still on its own ledger appears as a v0.5 peer section. The page stays read-only.

## Colors

### Primary
Violet accent marks next-step labels, navigation icons, selection and keyboard focus. The role is functional and restrained.

### Secondary
Red names attention, amber names waiting or health problems, and green marks successful checkpoints. Status text supplies meaning independently of hue.

### Neutral
Page, surface, hover, ink, secondary, muted and rule form a cool neutral hierarchy. The frontmatter records both OS themes directly from the shipping CSS.

**The Status Words Rule.** An icon and color always accompany a written status.

## Typography

One system-ui family, with platform fallbacks and tabular numerals. View headings are compact; row titles use medium weight, metadata stays regular, and detail titles provide the strongest scale change. Monospace is reserved for commands and report paths. Full detail prose is bounded at 72ch; list previews use a single visual line and ellipsis.

## Layout

The desktop shell has a 216px navigation rail. A selected row adds a 440px detail pane; at 1200px and below details replace the list. At 700px and below navigation becomes horizontal and machine filtering uses a native select. Desktop content gutters are 24px; phone gutters are 16px. Lists use a consistent 16px icon column, flexible text, status/machine metadata and aligned ages. Section gaps are 18px.

**The Complete Detail Rule.** A compact list preview must open the complete ledger text.

**The Global Counts Rule.** Local filters never rewrite global attention counts or imply that hidden work is clear.

## Elevation & Depth

Flat tonal layers and thin dividers define the shell, groups and selected rows. No shadows or decorative entrance motion. A focused detail pane scrolls natively on desktop and becomes a document on phone.

## Shapes

Small control corners and the compact workspace mark soften a rectangular list system. The shared 16px SVG grid uses a 1.6px rounded stroke. Status marks have distinct silhouettes; visible focus uses a 2px inset outline.

## Components

### Navigation
Neutral full-width buttons, selected fill, shared SVG and aligned counts. Names and counts remain explicit; machine identity is separate from campaign identity. Phone navigation keeps three real views.

### Work row
Compact button inside a semantic list. Title/preview and status/machine metadata form two tiers. Hover uses the neutral hover surface; selection uses the violet-neutral selection surface. Enter opens focused details; Escape or Back returns focus to the source row.

### Search and filters
Native search input and select, labelled scope buttons with aria-pressed. Active filters disclose their local scope and a clear-filters action. These controls never mutate the ledger.

### Details and disclosure
Wrapping full text, plain property pairs, native disclosure for decisions, handovers and history. Commands and paths use selectable monospace text. Removed items and incomplete snapshots say so explicitly.

## Do's and Don'ts

### Do:
- **Do** keep status words, machine identity, keyboard focus and full detail text reachable.
- **Do** use the same compact controls and neutral surfaces in both OS themes.
- **Do** distinguish snapshot health, server truncation and local filters.

### Don't:
- **Don't** expand long ledger paragraphs into the default work list.
- **Don't** add ledger-write controls or pretend to focus a remote pane.
- **Don't** render anything as HTML or fetch remote assets. Ledger strings are shown as plain text; captured documents are rendered from Markdown into DOM nodes through an allow-list.

## Campaign archive and documents

Full ledger views inherit the system font, compact status words, restrained accent links, OS themes and thin rules. Their order is goal, plan, named documents, decisions, handovers, then lanes. Document prose is bounded at 72ch; code and lane tables scroll within their own containers on narrow screens. View titles and Markdown h1 use 19px/600, with document headings stepping down at 17px, 15px and 13px so they never outrank the view title.

Lane rows disclose their task ID and indent descendants. Parent names appear when available on the page, with task IDs as the fallback. Archive paging uses Newer/Older; the ascending lane table uses Previous/Next. Misses and rendering-fallback notices use the muted prose treatment, preserving selectable source paths and complete preformatted text. Version links use the existing accent and visible keyboard focus.
