//! The one key table. The footer hints and the help overlay both read it, so the two
//! cannot disagree. Bindings themselves arrive with the event loop in P2.

pub(crate) const GLANCE: u8 = 1;
pub(crate) const CAMPAIGN: u8 = 2;
pub(crate) const PAGER: u8 = 4;
pub(crate) const SLOTR: u8 = 8;
const ALL: u8 = GLANCE | CAMPAIGN | PAGER | SLOTR;

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
    key(
        "Move",
        "tab",
        "next",
        "next section, then the detail",
        GLANCE,
    ),
    key("Move", "tab", "next", "next pane; shift-tab back", CAMPAIGN),
    key("Move", "1-5", "jump", "jump to a pane", CAMPAIGN),
    key("Move", "wheel", "scroll", "scroll", ALL),
    key(
        "Go",
        "⏎",
        "go",
        "go to the row's agent pane",
        GLANCE | CAMPAIGN | SLOTR,
    ),
    key("Go", "l", "open", "open the campaign", GLANCE),
    key("Go", "l", "open", "open the row's campaign", SLOTR),
    key("Go", "h", "back", "back, close (esc too)", ALL),
    key(
        "Go",
        "/",
        "filter",
        "filter rows by name",
        GLANCE | CAMPAIGN,
    ),
    key("Go", "/", "search", "search", PAGER),
    key("Go", "c", "all", "all campaigns, closed too", GLANCE),
    key(
        "Go",
        "s",
        "slotr",
        "slotr holders and queue",
        GLANCE | CAMPAIGN | SLOTR,
    ),
    key(
        "Go",
        "click",
        "select",
        "select; double-click is ⏎",
        GLANCE | CAMPAIGN | SLOTR,
    ),
    key(
        "Act",
        "a",
        "answer",
        "answer the selected ask",
        GLANCE | CAMPAIGN,
    ),
    key(
        "Act",
        "p",
        "park",
        "park or unpark (confirms)",
        GLANCE | CAMPAIGN,
    ),
    key(
        "Act",
        "o",
        "report",
        "read the report or doc",
        GLANCE | CAMPAIGN,
    ),
    key("Act", "y", "copy", "copy the jump command", ALL),
    key(
        "Act",
        "r",
        "refresh",
        "refresh now",
        GLANCE | CAMPAIGN | SLOTR,
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
    key("View", "q", "quit", "close; quits on the glance", ALL),
];

/// How the glance's detail pane is read; the help says so under "Move".
pub(crate) const DETAIL_NOTE: &str = "shift-tab goes back. In the wide view, with the detail \
    focused, j k g G ctrl-d ctrl-u PgDn PgUp scroll it; h or esc returns to the list.";

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
