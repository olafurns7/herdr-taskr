//! The typed ledger reads the view draws from. Field names follow `taskr --json glance`
//! (glance.go) and `taskr --json campaign ID` (cmd_campaign.go, P1b). [`RootRow`] has no
//! read behind it yet: the view builds those rows from the glance.
//!
//! Every struct is `#[serde(default)]`: a hub that is one release behind or ahead must
//! still decode.

use serde::Deserialize;

/// `taskr --json glance`: the owner snapshot.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Glance {
    pub now: String,
    /// `needs_you`, `attention`, `rolling` or `unknown`. The view counts the rows itself
    /// for the pill.
    pub verdict: String,
    /// Open owner asks: the only red rows.
    pub needs_you: Vec<Need>,
    /// Amber rows: the owner cannot trust what they see, or a campaign may be stuck unseen.
    pub attention: Vec<Attention>,
    /// Open campaigns, most recently active first; parked ones carry `parked`.
    pub campaigns: Vec<CampaignRow>,
    pub quiet: Quiet,
    /// The dim migration count: notes that still carry `OWNER:` items (P1a).
    pub owner_notes_pending: u32,
    /// The hub's own host name. Empty from a taskr older than P1b, and then the view
    /// cannot tell which rows are on its own Herdr.
    pub server_host: String,
    /// The hub's label for the caller, empty on the hub itself. A row whose `host`
    /// equals it is on this machine; hub rows carry an empty `host`.
    pub caller_host: String,
}

impl Glance {
    /// Whether a row on `host` is on the machine this view runs on, so its pane can be
    /// focused here. Unknown, and so false, until the snapshot names its server.
    pub fn local(&self, host: &str) -> bool {
        !self.server_host.is_empty() && host == self.caller_host
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Need {
    pub kind: String,
    pub campaign: String,
    pub root_id: i64,
    pub host: String,
    pub pane_id: String,
    pub age_ms: i64,
    pub since: String,
    pub ask_id: i64,
    pub text: String,
    pub blocking: bool,
    pub asker: String,
    pub asker_task_id: i64,
    pub asker_waiting: bool,
    pub also: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Attention {
    /// `lead_gone`, `lead_blocked`, `lead_idle_results`, `lead_unregistered_silent`,
    /// `parked_active`, or a visibility kind (daemon, host).
    pub kind: String,
    pub campaign: String,
    pub root_id: i64,
    pub text: String,
    pub age_ms: i64,
    pub since: String,
    pub host: String,
    pub pane_id: String,
    pub count: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CampaignRow {
    pub id: i64,
    pub name: String,
    pub host: String,
    pub pane_id: String,
    pub lanes: LaneCounts,
    /// `working`, `idle`, `gone`, `blocked` or `unregistered`.
    pub lead: String,
    /// The lead holds a live wait lease: it is waiting on its lanes.
    pub lead_waiting: bool,
    /// The latest milestone (ready, done, decision or note).
    pub last: Option<Last>,
    /// The newest owner note.
    pub owner_note: Option<Last>,
    pub parked: bool,
    pub parked_active: bool,
    pub park_age_ms: i64,
    pub activity_age_ms: i64,
    /// Events in the tree per ten minutes, oldest first, 24 buckets (P1b).
    pub spark: Vec<u32>,
}

impl CampaignRow {
    /// The row's second line: the owner note without its `OWNER: nothing.` segment, or the
    /// latest milestone when nothing is left.
    pub fn status_text(&self) -> &str {
        let note = self
            .owner_note
            .as_ref()
            .map_or("", |n| strip_owner_nothing(&n.text));
        if !note.is_empty() {
            return note;
        }
        self.last
            .as_ref()
            .map_or("", |l| strip_owner_nothing(&l.text))
    }
}

/// Drops a leading `OWNER: nothing.` segment; what is left is empty when the note said only
/// that, or only "nothing.".
fn strip_owner_nothing(text: &str) -> &str {
    let text = text.trim();
    let rest = text
        .strip_prefix("OWNER:")
        .map(str::trim_start)
        .and_then(|r| r.strip_prefix("nothing"))
        .map_or(text, |r| r.trim_start_matches(['.', ';', ' ']));
    if rest.trim_end_matches('.').eq_ignore_ascii_case("nothing") {
        ""
    } else {
        rest
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LaneCounts {
    pub working: u32,
    pub ready: u32,
    pub open: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Last {
    pub event_id: i64,
    pub kind: String,
    pub text: String,
    pub age_ms: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Quiet {
    pub count: u32,
    pub names: Vec<String>,
    /// The folded roots, in the order of `names`.
    pub root_ids: Vec<i64>,
}

/// `taskr campaign ROOT` (P1b): one campaign in full.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Campaign {
    pub root: Root,
    /// The goal's title line, then its first sentence.
    pub goal: Vec<String>,
    pub plan: PlanInfo,
    pub lanes: Vec<Lane>,
    /// Open asks first, then recent ask and answer pairs.
    pub asks: Vec<Event>,
    /// Decisions in force.
    pub decisions: Vec<Event>,
    pub docs: Vec<DocRow>,
    pub prs: Vec<Pr>,
    /// The tree's recent events, newest first; owner notes carry `owner`.
    pub log: Vec<Event>,
    pub spark: Vec<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Root {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub created_at: String,
    pub host: String,
    pub pane_id: String,
    pub next: String,
    pub lead: String,
    pub age_ms: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PlanInfo {
    pub version: u32,
    pub decisions_since: u32,
    pub closed_since: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Lane {
    pub id: i64,
    pub parent_id: i64,
    pub depth: u32,
    pub name: String,
    pub role: String,
    /// `open` or `closed`.
    pub status: String,
    /// An open lane's agent: `working`, `ready`, `idle` or `blocked`.
    pub state: String,
    /// A closed lane's outcome: `accepted`, `reworked`, `rejected`, `abandoned` or empty.
    pub outcome: String,
    pub host: String,
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub pane_id: String,
    /// The last ready, done or fail summary.
    pub summary: String,
    pub age_ms: i64,
    /// Whether the brief and the report were captured.
    pub brief: bool,
    pub report: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Event {
    pub id: i64,
    pub kind: String,
    pub at: String,
    pub lane: String,
    pub text: String,
    /// An ask that is still open.
    pub open: bool,
    /// An owner ask or an owner note.
    pub owner: bool,
    pub blocking: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct DocRow {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub lane: String,
    pub version: u32,
    /// False when the document was registered but its text was never captured.
    pub captured: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Pr {
    pub task_id: i64,
    /// One row per PR-valued ref: `pr`, or `pr.<slice>` for a lane with several.
    pub key: String,
    /// The ref as written; `number` only when it is all digits.
    pub value: String,
    pub number: u32,
    pub title: String,
    /// `open`, `merged` or `closed`.
    pub state: String,
    /// `pass`, `fail`, `running` or empty. Title, state, CI and review come from stored
    /// `pr.*` refs and only on the bare `pr` row; empty means unknown, not "none".
    pub ci: String,
    pub review: String,
    pub lane: String,
}

/// One row of the all-campaigns list: every root, closed ones too.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RootRow {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub parked: bool,
    pub host: String,
    pub lanes_open: u32,
    pub lanes_total: u32,
    pub activity_age_ms: i64,
}

/// One captured document: its row from the campaign read, and the raw text `taskr doc get
/// ID` prints.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Doc {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub lane: String,
    pub version: u32,
    pub body: String,
}

/// Everything the seven screens read. P2's live client fills the same struct.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Data {
    pub glance: Glance,
    pub campaign: Campaign,
    pub roots: Vec<RootRow>,
    pub doc: Doc,
    pub slotr: Slotr,
}

/// `taskr slotr --json`: `slotr status --json` (schema 1) as the hub reads it, plus
/// `available`, `host`, `now` and, on rows whose task is a ledger id, `root_id` and
/// `root_name`. Nullable fields are `Option`; `kind` and `priority` arrive with slotr's H1.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Slotr {
    pub available: bool,
    pub error: String,
    /// Where the rows are: empty for the hub, as the glance's hub rows.
    pub host: String,
    /// The hub's clock when it read slotr; ages are taken against it.
    pub now: String,
    pub schema_version: u32,
    /// By name, so in name order.
    pub pools: std::collections::BTreeMap<String, Pool>,
    pub stats: SlotrStats,
    pub last_stop: Option<LastStop>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Pool {
    pub slots: u32,
    pub holders: Vec<SlotRow>,
    /// In admission order.
    pub queue: Vec<SlotRow>,
    pub budget: Budget,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Budget {
    pub reserve_mib: f64,
    pub outstanding_mib: f64,
    /// What would be left after one more run of the pool's default cost, before the
    /// reserve: one more fits when this is at least `reserve_mib`. Null when slotr cannot
    /// read free memory.
    pub projected_free_mib: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
/// Each figure is null when slotr cannot read it (a kernel without PSI, say).
pub struct SlotrStats {
    pub available_mib: Option<f64>,
    pub total_mib: Option<f64>,
    pub psi_full_avg10: Option<f64>,
    pub psi_full_avg60: Option<f64>,
    pub load1: Option<f64>,
    pub cores: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LastStop {
    pub run: String,
    pub reason: String,
    pub at: String,
}

/// A holder or a waiter; the fields of the other kind stay empty.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SlotRow {
    pub run: String,
    pub campaign: String,
    pub task: String,
    pub pane: String,
    pub purpose: String,
    pub kind: Option<String>,
    /// H1's flag; any truthy value marks the row.
    pub priority: serde_json::Value,
    pub cost_mib: f64,
    pub anon_mib: Option<f64>,
    /// When it queued.
    pub since: Option<String>,
    pub admitted_at: Option<String>,
    pub lease_expires_at: Option<String>,
    pub warned_at: Option<String>,
    pub stopping_at: Option<String>,
    pub state: String,
    pub position: u32,
    pub wait_reason: String,
    /// Who holds the old heavy flock, on a `legacy_lock` wait.
    pub legacy_holder_pid: Option<i64>,
    /// The run's place in line; it keeps it when admitted, so the cursor can follow it.
    pub enqueue_seq: u64,
    pub root_id: i64,
    pub root_name: String,
}

impl SlotRow {
    pub fn priority(&self) -> bool {
        match &self.priority {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
            serde_json::Value::String(s) => !matches!(s.as_str(), "" | "normal" | "false"),
            _ => false,
        }
    }

    pub fn stopping(&self) -> bool {
        self.state == "stopping" || self.stopping_at.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slotr_sample_decodes_with_and_without_the_h1_fields() {
        // Synthetic, in the shape of `taskr slotr --json` over slotr 0.1.0 (schema 1).
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../tests/slotr-sample.json")).unwrap();
        let s: Slotr = serde_json::from_value(raw.clone()).unwrap();
        assert!(s.available && s.schema_version == 1 && s.host.is_empty());
        let pool = &s.pools["runtime"];
        let h = &pool.holders[0];
        assert_eq!(
            (pool.slots, h.run.as_str(), h.root_id),
            (2, "slotr-runtime-90", 1)
        );
        assert_eq!(
            (h.anon_mib, h.kind.as_deref(), h.priority()),
            (Some(5420.25), None, false)
        );
        assert!(h.stopping_at.is_none() && !h.stopping());
        assert_eq!(pool.budget.projected_free_mib, Some(-1740.8));
        assert_eq!((h.enqueue_seq, s.stats.cores), (30, Some(16.0)));
        let q: Vec<_> = pool
            .queue
            .iter()
            .map(|r| (r.position, r.wait_reason.as_str()))
            .collect();
        assert_eq!(q, [(1, "memory_budget"), (2, "fifo")]);
        assert_eq!(s.last_stop.unwrap().reason, "stop_psi_full_avg10");

        // H1 adds kind and priority to rows; anything else unknown is ignored.
        let mut h1 = raw;
        let row = &mut h1["pools"]["runtime"]["queue"][0];
        row["kind"] = "build".into();
        row["priority"] = true.into();
        h1["pools"]["runtime"]["holders"][0]["priority"] = false.into();
        h1["schema_version"] = 2.into();
        h1["future"] = serde_json::json!({"x": 1});
        // A figure slotr cannot read is null, not a failed read.
        h1["stats"]["psi_full_avg10"] = serde_json::Value::Null;
        h1["pools"]["runtime"]["budget"]["projected_free_mib"] = serde_json::Value::Null;
        let s: Slotr = serde_json::from_value(h1).unwrap();
        assert!(
            s.stats.psi_full_avg10.is_none()
                && s.pools["runtime"].budget.projected_free_mib.is_none()
        );
        let q = &s.pools["runtime"].queue[0];
        assert_eq!((q.kind.as_deref(), q.priority()), (Some("build"), true));
        assert!(!s.pools["runtime"].holders[0].priority());

        // taskr's answer when slotr cannot be read.
        let s: Slotr = serde_json::from_str(
            r#"{"available":false,"error":"no systemd user bus here","host":"","now":"2026-03-14T14:16:10.000Z"}"#,
        )
        .unwrap();
        assert!(!s.available && s.pools.is_empty() && s.error.starts_with("no systemd"));
    }

    #[test]
    fn the_p1b_sample_decodes() {
        // The synthetic sample from the P1b lane's report, in this crate's envelope.
        let d: Data = serde_json::from_str(include_str!("../tests/p1b-sample.json")).unwrap();
        let (g, c) = (&d.glance, &d.campaign);
        assert_eq!(
            (g.server_host.as_str(), g.caller_host.as_str()),
            ("demo-hub", "")
        );
        let ask = &g.needs_you[0];
        assert_eq!(
            (ask.asker_task_id, ask.pane_id.as_str(), ask.asker_waiting),
            (2, "wDemo:p2", true)
        );
        // The ask is on another machine; the campaign's lead is on the hub, where this is.
        assert!(!g.local(&ask.host) && g.local(&g.campaigns[0].host));
        let row = &g.campaigns[0];
        assert_eq!(
            (
                row.spark.len(),
                row.last.as_ref().unwrap().event_id,
                row.lead_waiting
            ),
            (24, 19, true)
        );
        assert_eq!((g.quiet.count, &g.quiet.root_ids), (1, &vec![3]));
        assert_eq!(
            (
                c.root.id,
                c.goal.len(),
                c.plan.version,
                c.plan.decisions_since
            ),
            (1, 2, 1, 1)
        );
        let lane = &c.lanes[0];
        assert_eq!(
            (lane.parent_id, lane.state.as_str(), lane.brief, lane.report),
            (1, "working", true, true)
        );
        assert_eq!(
            (
                c.asks.len(),
                c.decisions.len(),
                c.docs[1].kind.as_str(),
                c.log.len()
            ),
            (1, 1, "brief", 1)
        );
        // One row per PR-valued ref: the bare key, then a slice whose value is not a number.
        assert_eq!(
            (
                c.prs[0].key.as_str(),
                c.prs[1].key.as_str(),
                c.prs[1].number
            ),
            ("pr", "pr.backend", 0)
        );
        let pr = &c.prs[0];
        assert_eq!(
            (
                pr.number,
                pr.value.as_str(),
                pr.review.as_str(),
                pr.state.as_str()
            ),
            (123, "123", "approved", "")
        );
        // Before P1b there is no server name, and nothing is known to be local.
        assert!(!Glance::default().local(""));
    }

    #[test]
    fn owner_nothing_is_stripped_and_falls_back() {
        for (note, want) in [
            ("OWNER: nothing. NOW: plan sent", "NOW: plan sent"),
            ("OWNER: nothing.", ""),
            ("nothing.", ""),
            ("OWNER: merge #4", "OWNER: merge #4"),
            ("plain note", "plain note"),
        ] {
            assert_eq!(strip_owner_nothing(note), want, "{note}");
        }
        let last = |text: &str| {
            Some(Last {
                text: text.into(),
                ..Last::default()
            })
        };
        let row = CampaignRow {
            owner_note: last("OWNER: nothing."),
            last: last("S4 merged"),
            ..CampaignRow::default()
        };
        assert_eq!(row.status_text(), "S4 merged");
    }
}
