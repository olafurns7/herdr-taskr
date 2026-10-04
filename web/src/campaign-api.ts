export interface PageInfo { page: number; pages: number; total: number }
export interface DocumentMeta {
  doc_id: number; task_id: number; kind: string; name: string; version: number;
  captured: boolean; reason: string | null; source_path: string | null; source_host: string | null;
  bytes: number | null; format: string | null; created_at: string; event_id: number | null; backfill: number;
}
export interface DocumentView extends DocumentMeta {
  root_id: number; lane: string; sha256: string | null; captured_at: string | null; body?: string;
  versions: { id: number; version: number; created_at: string; event_id: number | null }[];
}
export interface CampaignRoot {
  id: number; name: string; role: string; status: string; host: string; created_at: string; closed_at: string; next: string;
}
export interface CampaignLane extends Omit<CampaignRoot, "next"> {
  parent_id: number; depth: number; provider: string; model: string; effort: string; summary: string;
  brief: DocumentMeta | null; report: DocumentMeta | null;
}
export interface CampaignNote {
  id: number; text: string; at: string; age_ms: number; owner: boolean;
}
export interface CampaignView extends PageInfo {
  root: CampaignRoot; goal: DocumentMeta | null; goal_miss?: DocumentMeta;
  plan: (DocumentMeta & { decisions_since: number; closed_since: number }) | null;
  documents: DocumentMeta[]; decisions: { id: number; time: string; text: string }[];
  handovers: { id: number; time: string; note: string; doc_id: number | null }[]; lanes: CampaignLane[];
  notes?: CampaignNote[];
}
export interface CampaignArchive extends PageInfo {
  campaigns: { id: number; name: string; status: string; created_at: string; closed_at: string; lane_counts: Record<string, number>; goal: string }[];
}
export async function fetchCampaignData<T>(path: string, signal?: AbortSignal): Promise<T> {
  const response = await fetch(path, { cache: "no-store", credentials: "same-origin", signal });
  if (!response.ok) throw new Error(response.status === 404 ? "Not found in this ledger." : "Cannot read the ledger (HTTP " + response.status + ").");
  return await response.json() as T;
}
