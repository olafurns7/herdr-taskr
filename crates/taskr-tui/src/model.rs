//! The typed ledger reads the view draws from. Field names follow `taskr --json glance`
//! (glance.go) and the fields P1a and P1b add; the `taskr campaign` read is P1b and its
//! names here are this crate's proposal until that read ships.
//!
//! Every struct is `#[serde(default)]`: a hub that is one release behind or ahead must
//! still decode.

use serde::Deserialize;

/// `taskr --json glance`: the owner snapshot.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Glance {
    pub now: String,
    /// `needs_you`, `attention` or `ok`. The view counts the rows itself for the pill.
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
    /// The hub's own host label, for the header (P1b).
    pub server_host: String,
    /// The hub's label for the caller; a row whose `host` equals it is local (P1b).
    pub caller_host: String,
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
    pub lane: String,
    pub lane_id: i64,
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
    pub with_backlog: u32,
    pub names: Vec<String>,
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

#[derive(Debug, Clone, Default, Deserialize)]
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
    pub number: u32,
    pub title: String,
    /// `open`, `merged` or `closed`.
    pub state: String,
    /// `pass`, `fail`, `running` or empty.
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

/// `taskr doc get`: one captured document.
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
