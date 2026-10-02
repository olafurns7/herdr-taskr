import type { Icon } from "../model";

// Marks: state before hue. One 16-unit grid, one 2px stroke, drawn here.
type Part = ["path" | "circle", Record<string, string>];
const PARTS: Record<Icon, Part[]> = {
  done: [["circle", { cx: "8", cy: "8", r: "6" }], ["path", { d: "M5 8l2 2 4-4" }]],
  failed: [["path", { d: "M4 4l8 8M12 4l-8 8" }]],
  working: [["circle", { cx: "8", cy: "8", r: "6" }], ["path", { d: "M8 2a6 6 0 0 1 0 12z", class: "solid" }]],
  planned: [["circle", { cx: "8", cy: "8", r: "6", "stroke-dasharray": "2 2" }]],
  ready: [["circle", { cx: "8", cy: "8", r: "6" }], ["path", { d: "M5.3 8.2l1.9 1.9 3.6-3.8" }]],
  blocked: [["circle", { cx: "8", cy: "8", r: "6" }], ["path", { d: "M4.9 8h6.2" }]],
  missing: [["circle", { cx: "8", cy: "8", r: "6", "stroke-dasharray": "2.4 2.2" }]],
  flag: [["path", { d: "M4 14.5V2" }], ["path", { d: "M4 2.6h8.2l-2.1 3 2.1 3H4", class: "solid" }]],
  note: [["path", { d: "M3 5h10M3 8h10M3 11h6" }]],
  handover: [["path", { d: "M2 8h8M7 4.8L10.2 8 7 11.2M13 3v10" }]],
  adopt: [["path", { d: "M14 8H6M9 4.8L5.8 8 9 11.2M3 3v10" }]],
  decision: [["path", { d: "M8 2l6 6-6 6-6-6z" }]],
  open: [["path", { d: "M6 3.5L10.5 8 6 12.5" }]],
  ref: [["path", { d: "M2.5 2.5h5.6l5.4 5.4-5.6 5.6-5.4-5.4z" }], ["circle", { cx: "5.5", cy: "5.5", r: "1", class: "solid" }]],
  inbox: [["path", { d: "M2 9l2-6h8l2 6v4H2zM2 9h4l1 2h2l1-2h4" }]],
  campaign: [["path", { d: "M2 4h5l2 2h5v7H2z" }]],
  activity: [["path", { d: "M1 8h3l2-5 4 10 2-5h3" }]],
  machine: [["path", { d: "M2 2h12v9H2zM5 14h6M8 11v3" }]],
  search: [["circle", { cx: "6.5", cy: "6.5", r: "4.5" }], ["path", { d: "M10 10l4 4" }]],
  back: [["path", { d: "M7 3L2 8l5 5M2 8h12" }]],
};

export function Mark({ icon, label }: { icon: Icon; label?: string }) {
  return (
    <svg class="mark" viewBox="0 0 16 16" role={label ? "img" : undefined} aria-label={label} aria-hidden={label ? undefined : "true"}>
      {PARTS[icon].map(([tag, attrs], i) => (tag === "path" ? <path key={i} {...attrs} /> : <circle key={i} {...attrs} />))}
    </svg>
  );
}
