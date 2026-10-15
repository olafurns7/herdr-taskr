//! The one key table. The footer hints and the help overlay both read it, so the two
//! cannot disagree. Bindings themselves arrive with the event loop in P2.

pub(crate) const GLANCE: u8 = 1;
pub(crate) const CAMPAIGN: u8 = 2;
pub(crate) const PAGER: u8 = 4;
pub(crate) const SLOTR: u8 = 8;
/// The Asks and Fleet screens.
pub(crate) const LIST: u8 = 16;
pub(crate) const DETAIL: u8 = 32;
const ALL: u8 = GLANCE | CAMPAIGN | PAGER | SLOTR | LIST | DETAIL;

pub(crate) struct Key {
    pub keys: &'static str,
    /// The footer's word for it.
    pub hint: &'static str,
    pub help: &'static str,
    pub group: &'static str,
    pub views: u8,
}

const fn key(
    group: &'static str,
    keys: &'static str,
    hint: &'static str,
    help: &'static str,
    views: u8,
) -> Key {
    Key {
        keys,
        hint,
        help,
        group,
        views,
    }
}

pub(crate) const GROUPS: [&str; 4] = ["Move", "Go", "Act", "View"];

pub(crate) const KEYS: &[Key] = &[
    key("Move", "j k", "move", "down, up (arrows too)", ALL),
    key("Move", "g G", "ends", "first, last row", ALL),
    key("Move", "ctrl-d ctrl-u", "half page", "half page", ALL),
    key("Move", "space", "page", "page down", PAGER),
    key("Move", "tab", "next", "next card; shift-tab back", GLANCE),
    key("Move", "tab", "next", "next pane; shift-tab back", CAMPAIGN),
    key(
        "View",
        "0-9",
        "panels",
        "toggle asks, checks, campaigns, PRs, Shipped, Build, Decisions, Fleet, Decision log, \
         0 Issues (+ shown, - hidden)",
        GLANCE,
    ),
    key(
        "View",
        "H",
        "health",
        "toggle Health, the hub's production report (h stays back)",
        GLANCE,
    ),
    key("Move", "1-5", "jump", "jump to a pane", CAMPAIGN),
    key("Move", "wheel", "scroll", "scroll", ALL),
    key(
        "Go",
        "D A F I",
        "screens",
        "Dashboard, Asks, Fleet; I: the selected row in full (Detail)",
        ALL,
    ),
    key("Go", "[ ]", "cycle", "previous, next screen", ALL),
    key(
        "Go",
        "⏎",
        "go",
        "go to the row's agent pane; on Asks, answer",
        GLANCE | LIST | DETAIL | CAMPAIGN | SLOTR,
    ),
    key(
        "Go",
        "l",
        "open",
        "open the campaign",
        GLANCE | LIST | DETAIL,
    ),
    key("Go", "l", "open", "open the row's campaign", SLOTR),
    key("Go", "h", "back", "back, close (esc too)", ALL),
    key(
        "Go",
        "/",
        "filter",
        "filter rows by name",
        GLANCE | LIST | CAMPAIGN,
    ),
    key("Go", "/", "search", "search", PAGER),
    key("Go", "c", "all", "all campaigns, closed too", GLANCE | LIST),
    key(
        "Go",
        "s",
        "slotr",
        "slotr holders and queue",
        GLANCE | LIST | DETAIL | CAMPAIGN,
    ),
    key("Go", "s", "back", "close slotr (h, esc too)", SLOTR),
    key(
        "Go",
        "click",
        "select",
        "select; double-click is ⏎",
        GLANCE | LIST | CAMPAIGN | SLOTR,
    ),
    key("Act", "a", "answer", "answer this ask, else pick one", ALL),
    key(
        "Act",
        "p",
        "park",
        "park or unpark (confirms)",
        GLANCE | LIST | DETAIL | CAMPAIGN,
    ),
    key("Act", "o", "report", "read the report or doc", CAMPAIGN),
    key(
        "Act",
        "o",
        "open",
        "Decisions, Fleet or log row: open (re-read) its campaign",
        GLANCE | DETAIL,
    ),
    key("Act", "y", "copy", "copy the jump command", ALL),
    key(
        "Act",
        "r",
        "refresh",
        "refresh now",
        GLANCE | LIST | DETAIL | CAMPAIGN | SLOTR,
    ),
    key("View", "?", "help", "this help", ALL),
    key("View", "m", "mouse", "mouse on or off", ALL),
    key("View", "t", "theme", "theme: dark, light, terminal", ALL),
    key(
        "View",
        "ctrl-l",
        "redraw",
        "clear and redraw the screen",
        ALL,
    ),
    key(
        "View",
        "q",
        "quit",
        "close; quits on a top-level screen",
        ALL,
    ),
];

/// The top-level screens; the help says so under "Go".
pub(crate) const SCREENS_NOTE: &str = "Each screen keeps its place; switching reads \
    nothing. 0-9 and H shape the Dashboard only. Detail scrolls; esc or h returns to its row.";

/// What panels 7-9, 0 and H show and what refreshes them; the glance's help says so under
/// "Go".
pub(crate) const LEDGER_NOTE: &str = "7, 9: the campaign last opened with l, as cached; l \
    or o reads it again, r does not. 8: listed campaigns only; / filters it, not 7 or 9. \
    0: issues_repo issues by current assignee, read at most every 5 min while shown; \
    r does not hurry it. H: the latest HEALTH note on hub root health_root for health_repo production, \
    as of its checked time, not live health; read at most once a minute while shown. \
    Set repos (4-6), issues_repo (0), health_repo and health_root (H) in ~/.config/taskr/tui.json. \
    Unset scopes read nothing.";

/// The answer dialog's own keys; the help says so under "Act".
pub(crate) const ANSWER_NOTE: &str = "Answering: j k choose, tab types (a note after a \
    structured ask's option), ⏎ reviews, y sends. A multi-select ask: space picks, ⏎ reviews \
    the set.";

/// Enter moves more than this pane (counter-review P2-1); the help says so under "Go".
pub(crate) const GO_NOTE: &str = "⏎ moves every client attached to that Herdr server.";

/// A footer hint: the key and its word from the table, or `label` when the row changes
/// what the key does ("go to lead", "how to get there").
pub(crate) fn hint(
    keys: &'static str,
    label: Option<&'static str>,
) -> (&'static str, &'static str) {
    let key = KEYS
        .iter()
        .find(|k| k.keys == keys)
        .expect("a footer hint names a key in the table");
    (key.keys, label.unwrap_or(key.hint))
}
