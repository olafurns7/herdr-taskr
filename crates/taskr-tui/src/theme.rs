//! Thirteen semantic tokens mapped onto Herdr's palette (plan §3, Theme): Catppuccin Mocha
//! for dark, Latte for light, with 256-colour, 16-colour and no-colour fallbacks. The page
//! background is never painted.
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// An open owner ask (red). Only the pill, `NEEDS YOU` and its rows.
    pub need: Color,
    /// The owner cannot trust what they see (amber).
    pub check: Color,
    /// No owner action (green): the pill, "recommended" and confirmations.
    pub ok: Color,
    /// A lead or lane is working.
    pub work: Color,
    /// Ready or done.
    pub done: Color,
    /// Focus border, keys, cursor bar, sparkline.
    pub accent: Color,
    /// Another machine.
    pub remote: Color,
    pub text: Color,
    pub sub: Color,
    /// Quiet, parked, closed, hints.
    pub dim: Color,
    /// Rules and unfocused borders.
    pub rule: Color,
    /// Cursor row background.
    pub sel: Color,
    /// Dialog background, and the text on the verdict pill.
    pub panel: Color,
    /// Fills use reverse video: a 16-colour background cannot be trusted to contrast, and
    /// without colour there is no background at all.
    pub reverse: bool,
}

impl Theme {
    /// The cursor row, and a chosen option.
    pub fn selected(&self) -> Style {
        if self.reverse {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(self.sel)
        }
    }

    /// The verdict pill: the only filled colour block on the screen.
    pub fn pill(&self, color: Color) -> Style {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        if self.reverse {
            bold.fg(color).add_modifier(Modifier::REVERSED)
        } else {
            bold.fg(self.panel).bg(color)
        }
    }

    /// The text cursor in the answer field.
    pub fn cursor(&self) -> Style {
        if self.reverse {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(self.text)
        }
    }
}

const fn rgb(v: u32) -> Color {
    Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// Tokens in the order of the spec's table: need, check, ok, work, done, accent, remote,
/// text, sub, dim, rule, sel, panel.
const fn theme(c: [Color; 13], reverse: bool) -> Theme {
    Theme {
        need: c[0],
        check: c[1],
        ok: c[2],
        work: c[3],
        done: c[4],
        accent: c[5],
        remote: c[6],
        text: c[7],
        sub: c[8],
        dim: c[9],
        rule: c[10],
        sel: c[11],
        panel: c[12],
        reverse,
    }
}

const fn indexed(i: [u8; 13]) -> [Color; 13] {
    let mut out = [Color::Reset; 13];
    let mut n = 0;
    while n < 13 {
        out[n] = Color::Indexed(i[n]);
        n += 1;
    }
    out
}

pub const DARK: Theme = theme(
    [
        rgb(0xf38ba8),
        rgb(0xfab387),
        rgb(0xa6e3a1),
        rgb(0xf9e2af),
        rgb(0x94e2d5),
        rgb(0x89b4fa),
        rgb(0xcba6f7),
        rgb(0xcdd6f4),
        rgb(0xa6adc8),
        rgb(0x6c7086),
        rgb(0x45475a),
        rgb(0x313244),
        rgb(0x181825),
    ],
    false,
);

pub const LIGHT: Theme = theme(
    [
        rgb(0xd20f39),
        rgb(0xfe640b),
        rgb(0x40a02b),
        rgb(0xdf8e1d),
        rgb(0x179299),
        rgb(0x1e66f5),
        rgb(0x8839ef),
        rgb(0x4c4f69),
        rgb(0x6c6f85),
        rgb(0x9ca0b0),
        rgb(0xbcc0cc),
        rgb(0xbdd0f5),
        rgb(0xeff1f5),
    ],
    false,
);

/// The nearest xterm indexes to the truecolor values.
pub const DARK_256: Theme = theme(
    indexed([
        211, 216, 151, 223, 116, 111, 183, 189, 146, 243, 239, 237, 234,
    ]),
    false,
);
pub const LIGHT_256: Theme = theme(
    indexed([161, 202, 70, 172, 30, 27, 99, 240, 243, 248, 251, 153, 255]),
    false,
);

/// The host's own ANSI palette, as Herdr's `terminal` theme does.
pub const TERMINAL: Theme = theme(
    [
        Color::Red,
        Color::Yellow,
        Color::Green,
        Color::Yellow,
        Color::Cyan,
        Color::Blue,
        Color::Magenta,
        Color::Reset,
        Color::Reset,
        Color::DarkGray,
        Color::DarkGray,
        Color::Reset,
        Color::Reset,
    ],
    true,
);

/// `NO_COLOR` or not a terminal: the marks, the words in the pill, capitals, bold and
/// reverse video carry the meaning.
pub const PLAIN: Theme = theme([Color::Reset; 13], true);

pub const NAMES: [&str; 3] = ["dark", "light", "terminal"];

/// Picks the theme. `name` is `--theme` or `TASKR_THEME`; `light_background` is what the
/// terminal answered about its background, when it answered.
pub fn choose(
    name: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
    tty: bool,
    light_background: Option<bool>,
) -> Theme {
    let set = |key: &str| env(key).filter(|v| !v.is_empty());
    if !tty || set("NO_COLOR").is_some() {
        return PLAIN;
    }
    let name = name.map(str::to_string).or_else(|| set("TASKR_THEME"));
    let light = match name.as_deref() {
        Some("terminal") => return TERMINAL,
        Some("light") => true,
        Some("dark") => false,
        _ => light_background == Some(true),
    };
    let truecolor = matches!(set("COLORTERM").as_deref(), Some("truecolor" | "24bit"));
    let more = set("TERM").is_some_and(|t| t.contains("256color"));
    match (truecolor, more, light) {
        (true, _, false) => DARK,
        (true, _, true) => LIGHT,
        (_, true, false) => DARK_256,
        (_, true, true) => LIGHT_256,
        _ => TERMINAL,
    }
}

/// Whether a reply to the background query (OSC 11) names a light colour:
/// `ESC ] 11 ; rgb:RRRR/GGGG/BBBB`.
pub fn light_background(reply: &[u8]) -> Option<bool> {
    let text = String::from_utf8_lossy(reply);
    let rgb = text.split("]11;rgb:").nth(1)?;
    let mut parts = rgb.split('/').map(|p| {
        let hex: String = p
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .take(2)
            .collect();
        u32::from_str_radix(&hex, 16).ok()
    });
    let (r, g, b) = (parts.next()??, parts.next()??, parts.next()??);
    // Perceived brightness, 0 to 255000.
    Some(r * 299 + g * 587 + b * 114 > 127_500)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_picks_the_theme() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            }
        };
        let herdr: &[(&str, &str)] = &[("COLORTERM", "truecolor"), ("TERM", "xterm-256color")];
        assert_eq!(choose(None, env(herdr), true, None), DARK);
        assert_eq!(choose(None, env(herdr), true, Some(true)), LIGHT);
        assert_eq!(choose(Some("dark"), env(herdr), true, Some(true)), DARK);
        assert_eq!(choose(Some("terminal"), env(herdr), true, None), TERMINAL);
        assert_eq!(
            choose(
                None,
                env(&[("TERM", "screen-256color"), ("TASKR_THEME", "light")]),
                true,
                None
            ),
            LIGHT_256
        );
        assert_eq!(
            choose(None, env(&[("TERM", "linux")]), true, None),
            TERMINAL
        );
        assert_eq!(
            choose(
                Some("light"),
                env(&[("COLORTERM", "truecolor"), ("NO_COLOR", "1")]),
                true,
                None
            ),
            PLAIN
        );
        assert_eq!(
            choose(
                None,
                env(&[("COLORTERM", "truecolor"), ("NO_COLOR", "")]),
                true,
                None
            ),
            DARK
        );
        assert_eq!(choose(None, env(herdr), false, None), PLAIN);
    }

    #[test]
    fn the_background_reply_says_light_or_dark() {
        assert_eq!(
            light_background(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\"),
            Some(false)
        );
        assert_eq!(
            light_background(b"\x1b]11;rgb:efef/f1f1/f5f5\x07\x1b[?62c"),
            Some(true)
        );
        assert_eq!(light_background(b"\x1b]11;rgb:ff/ff/ff\x07"), Some(true));
        assert_eq!(light_background(b"\x1b[?62c"), None);
        assert_eq!(light_background(b"\x1b]11;rgb:zz"), None);
    }
}
