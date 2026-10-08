//! What the terminal shows after the diff matches a fresh frame, styles included.

use ratatui::{
    Terminal,
    backend::{Backend, ClearType, TestBackend, WindowSize},
    buffer::{Buffer, Cell},
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Position, Size},
    style::Color,
};
use taskr_tui::{App, Screen, draw, frame, frames, input, model::Data};
use unicode_width::UnicodeWidthStr;

/// A terminal as tmux 3.4 and Herdr's libghostty are: a cell advances the cursor by the
/// width of its whole symbol, so `⚠️` takes two columns. Cells are written where its cursor
/// is, and the cursor moves only when the backend says so: a cell ratatui writes right after
/// a wide one, without a move, lands where the terminal's cursor is, not where the buffer
/// put it.
struct Tmux {
    grid: Buffer,
    cursor: (u16, u16),
}

impl Tmux {
    fn new(width: u16, height: u16) -> Self {
        let area = ratatui::layout::Rect::new(0, 0, width, height);
        Self {
            grid: Buffer::empty(area),
            cursor: (0, 0),
        }
    }
}

impl Backend for Tmux {
    type Error = std::io::Error;

    fn draw<'a, I: Iterator<Item = (u16, u16, &'a Cell)>>(
        &mut self,
        content: I,
    ) -> std::io::Result<()> {
        // As the crossterm backend does: a cursor move unless the cell is next to the last.
        let mut last: Option<(u16, u16)> = None;
        for (x, y, cell) in content {
            if last != Some((x.wrapping_sub(1), y)) {
                self.cursor = (x, y);
            }
            last = Some((x, y));
            let (cx, cy) = self.cursor;
            let width = cell.symbol().width() as u16;
            // An empty cell prints nothing.
            if width == 0 {
                continue;
            }
            if cx < self.grid.area.width {
                // Writing over half of a wide glyph blanks the other half.
                if cx + 1 < self.grid.area.width && self.grid[(cx + 1, cy)].symbol().is_empty() {
                    self.grid[(cx + 1, cy)].reset();
                }
                if cx > 0 && self.grid[(cx, cy)].symbol().is_empty() {
                    self.grid[(cx - 1, cy)].reset();
                }
                self.grid[(cx, cy)] = cell.clone();
            }
            for covered in cx + 1..(cx + width).min(self.grid.area.width) {
                self.grid[(covered, cy)].reset();
                self.grid[(covered, cy)].set_symbol("");
            }
            self.cursor.0 = cx + width;
        }
        Ok(())
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn show_cursor(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn get_cursor_position(&mut self) -> std::io::Result<Position> {
        Ok(Position::new(self.cursor.0, self.cursor.1))
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
        let p = position.into();
        self.cursor = (p.x, p.y);
        Ok(())
    }
    fn clear(&mut self) -> std::io::Result<()> {
        self.grid.reset();
        Ok(())
    }
    fn clear_region(&mut self, clear: ClearType) -> std::io::Result<()> {
        assert_eq!(
            clear,
            ClearType::All,
            "the view clears the whole screen only"
        );
        self.clear()
    }
    fn size(&self) -> std::io::Result<Size> {
        Ok(self.grid.area.as_size())
    }
    fn window_size(&mut self) -> std::io::Result<WindowSize> {
        unimplemented!("the view asks for the size only")
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `app` painted whole on a blank tmux: what a full repaint shows there.
fn fresh_tmux(app: &App, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(Tmux::new(w, h)).expect("a terminal");
    t.draw(|f| draw(f, app)).expect("a frame");
    t.backend().grid.clone()
}

fn fresh(app: &App, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).expect("a test backend");
    t.draw(|f| draw(f, app)).expect("a frame");
    t.backend().buffer().clone()
}

/// The cells of `seen` that differ from `want`: glyph, colours and modifiers.
fn differences(seen: &Buffer, want: &Buffer) -> Vec<String> {
    let mut out = vec![];
    for y in 0..want.area.height {
        for x in 0..want.area.width {
            let (a, b) = (&seen[(x, y)], &want[(x, y)]);
            if (a.symbol(), a.fg, a.bg, a.modifier) != (b.symbol(), b.fg, b.bg, b.modifier) {
                out.push(format!("({x},{y}) {a:?} want {b:?}"));
            }
        }
    }
    out
}

fn app(data: Data) -> App {
    let mut app = App::new(data);
    app.fetch.loaded = true;
    app
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> input::Effect {
    match input::key(app, KeyEvent::new(code, modifiers)) {
        input::Effect::Run(job) => {
            input::demo(app, job);
            input::Effect::None
        }
        effect => effect,
    }
}

/// Live ledger text with emoji the terminal draws narrower than the layout: moving the
/// cursor over those rows, entering the campaign (its lanes hold them too) and coming back
/// leaves nothing of an earlier frame, glyph or colour. The diff alone, without the
/// repaint a new screen gets, must get there.
#[test]
fn wide_glyphs_leave_no_debris() {
    for (w, h) in [(120, 40), (46, 30)] {
        let mut app = app(frames::live_fixture());
        let mut terminal = Terminal::new(Tmux::new(w, h)).expect("a terminal");
        let keys = "j j k k l j j j j h j j j j j j j j j j j l k k k h";
        for key in std::iter::once("").chain(keys.split(' ')) {
            if let Some(c) = key.chars().next() {
                press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
            }
            terminal.draw(|f| draw(f, &app)).expect("a frame");
            let wrong = differences(&terminal.backend().grid, &fresh_tmux(&app, w, h));
            assert!(wrong.is_empty(), "{w}x{h} after {key:?}: {wrong:#?}");
        }
    }
}

/// A cell the terminal shows differently from the buffer (here, painted behind the view's
/// back) is gone after a trip into the campaign and back, and after Ctrl-L.
#[test]
fn a_new_screen_and_ctrl_l_paint_the_whole_frame() {
    let mut app = app(frames::fixture());
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("a test backend");
    let mut shown = None;
    // The `t` of " taskr" is the same glyph and colour on both screens: the diff alone
    // never writes it again.
    let mut drift = Cell::new("t");
    drift.set_fg(Color::Red);
    let poke = |terminal: &mut Terminal<TestBackend>| {
        terminal
            .backend_mut()
            .draw(std::iter::once((2, 0, &drift)))
            .expect("a poke");
    };
    frame(&mut terminal, &app, &mut shown).expect("a frame");
    press(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
    frame(&mut terminal, &app, &mut shown).expect("a frame");
    assert_eq!(app.screen, Screen::Campaign);
    poke(&mut terminal);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    frame(&mut terminal, &app, &mut shown).expect("a frame");
    assert_eq!(app.screen, Screen::Glance);
    let wrong = differences(terminal.backend().buffer(), &fresh(&app, 120, 40));
    assert!(wrong.is_empty(), "after back: {wrong:#?}");

    poke(&mut terminal);
    let effect = press(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert_eq!(effect, input::Effect::Redraw);
    shown = None;
    frame(&mut terminal, &app, &mut shown).expect("a frame");
    let wrong = differences(terminal.backend().buffer(), &fresh(&app, 120, 40));
    assert!(wrong.is_empty(), "after ctrl-l: {wrong:#?}");
}

/// Live text must never crash a view: every screen of the live fixture draws.
#[test]
fn wide_glyphs_draw_on_every_screen() {
    for (w, h) in [(46, 30), (120, 40)] {
        for screen in [
            Screen::Glance,
            Screen::Campaign,
            Screen::AllCampaigns,
            Screen::Help,
        ] {
            let mut app = app(frames::live_fixture());
            app.screen = screen;
            let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("a test backend");
            terminal.draw(|f| draw(f, &app)).expect("a frame");
        }
        let mut app = app(frames::live_fixture());
        app.answer(app.data.glance.needs_you[0].clone());
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("a test backend");
        terminal.draw(|f| draw(f, &app)).expect("a frame");
    }
}
