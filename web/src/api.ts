// The /api/state contract, mirroring the Go structs in dashboard.go and
// hub.go field for field. Fields the server always sends are required;
// `omitempty` fields are optional; kinds stay `string` on the wire and are
// narrowed in model.ts. src/fixtures/state.json (written by a Go test from a
// seeded ledger) is checked against these types by `pnpm typecheck`.

export interface TaskRef {
  id: number;
  name: string;
  role?: string;
}

export interface Stamped {
  text: string;
  at: string;
  age_ms: number;
}

export interface LastEvent {
  kind: string;
  summary?: string;
  at: string;
  age_ms: number;
}

export interface PlanRef {
  key: string;
  value: string;
}

export interface PlanDecision {
  id: number;
  kind: string; // decision | owner_ask
  text: string;
  answer?: string;
  answer_id?: number;
  from?: string;
  at: string;
  age_ms: number;
}

export interface OwnerAsk {
  id: number;
  text: string;
  at: string;
  age_ms: number;
  blocking: boolean;
  asker: TaskRef;
  root: TaskRef;
  asker_is_root: boolean;
  asker_waiting: boolean;
  workspace_id?: string;
  tab_id?: string;
  pane_id?: string;
}

export interface StateTask {
  id: number;
  parent_id: number;
  depth: number;
  name: string;
  role: string;
  status: string;
  waiting: boolean;
  agent_status?: string;
  observed_at?: string;
  present?: boolean;
  last_event?: LastEvent;
  open_asks: number;
  report_path?: string;
  pane_id?: string;
  next?: Stamped;
  refs?: PlanRef[];
  agent_name?: string;
  mark?: string;
}

export interface UnreadWork {
  event_id: number;
  kind: string;
  summary?: string;
  at: string;
  age_ms: number;
  count: number;
  recipient: TaskRef;
  from: TaskRef;
}

export interface Orchestrator {
  id: number;
  name: string;
  role: string;
  status: string;
  workspace_id?: string;
  tab_id?: string;
  pane_id?: string;
  created_at: string;
  waiting: boolean;
  unacked: number;
  open_asks: number;
  note: Stamped | null;
  last_event?: LastEvent;
  tasks: StateTask[];
  closed_tasks: number;
  tasks_truncated?: number;
  next?: Stamped;
  refs?: PlanRef[];
  decisions?: PlanDecision[];
  last_handover?: Stamped;
  unread_work?: UnreadWork[];
  tally?: string;
}

export interface Activity {
  id: number;
  kind: string;
  summary?: string;
  at: string;
  age_ms: number;
  task: TaskRef;
  root_id: number;
  root_name: string;
}

export interface ClosedRoot {
  id: number;
  name: string;
  closed_at: string;
  age_ms: number;
  note: Stamped | null;
}

export interface DaemonHealth {
  state: string; // fresh | stale | none
  at?: string;
  age_ms?: number;
}

export interface Milestone {
  id: number;
  kind: string;
  text: string;
  key?: string;
  at: string;
  age_ms: number;
  root_id: number;
  orchestrator: string;
  lane_id?: number;
  lane?: string;
  role?: string;
  machine?: string;
  node_id?: string;
}

export interface AttentionItem {
  kind: string;
  severity: number;
  machine: string;
  node_id?: string;
  local: boolean;
  orchestrator?: TaskRef;
  lane?: TaskRef;
  since?: string;
  age_ms: number;
  text: string;
  action: string;
  command?: string;
  pane_id?: string;
  report?: string;
  ask_id?: number;
  blocking?: boolean;
  asker_waiting?: boolean;
  also?: LaneSignal[]; // owner_ask: the asking lane's own cues, folded in
}

/** A lane cue folded into that lane's owner ask (dashboard.go foldIntoAsks). */
export interface LaneSignal {
  kind: string;
  since?: string;
  age_ms: number;
  text: string;
  action: string;
  command?: string;
  report?: string;
}

export interface DashState {
  now: string;
  version: string;
  owner_asks: OwnerAsk[];
  orchestrators: Orchestrator[];
  activity: Activity[];
  closed: ClosedRoot[];
  daemon?: DaemonHealth;
  milestones: Milestone[];
  attention: AttentionItem[];
}

export interface MachineView {
  machine: string;
  node_id?: string;
  local: boolean;
  last_push_age_ms: number;
  stale: boolean;
  state: DashState;
}

export interface NeedAsk extends OwnerAsk {
  machine: string;
  node_id?: string;
  local: boolean;
  stale: boolean;
}

/** /api/state: this machine's own fields, then every machine merged. */
export interface StateView extends DashState {
  machine: string;
  hub: boolean;
  machines: MachineView[];
  needs_you: NeedAsk[];
}

export async function fetchState(): Promise<StateView> {
  const res = await fetch("/api/state", { cache: "no-store", credentials: "same-origin" });
  if (!res.ok) throw new Error("HTTP " + res.status);
  return (await res.json()) as StateView;
}
