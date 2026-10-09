//! H7 P2 lifecycle: a background Slotr reply must touch only the Slotr cursor.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use taskr_tui::{
    App, Screen,
    actions::{Done, Job},
    frames, input,
};
fn key(app: &mut App, c: char) {
    input::key(app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
}
fn finish(app: &mut App, next: taskr_tui::model::Slotr) {
    input::done(app, &Job::Slotr, Ok(Done::Slotr(Box::new(next))));
}
#[test]
fn late_reply_after_close_preserves_glance_cursor() {
    let mut app = App::new(frames::fixture());
    app.row = 4;
    key(&mut app, 's');
    key(&mut app, 's');
    assert_eq!((app.screen, app.row), (Screen::Glance, 4));
    let mut next = app.data.slotr.clone();
    next.pools.get_mut("runtime").unwrap().holders.remove(0);
    finish(&mut app, next);
    assert_eq!((app.screen, app.row), (Screen::Glance, 4));
}
#[test]
fn reply_under_help_follows_saved_slotr_cursor() {
    let mut app = App::new(frames::fixture());
    key(&mut app, 's');
    app.row = 4;
    key(&mut app, '?');
    let mut next = app.data.slotr.clone();
    next.pools.get_mut("runtime").unwrap().holders.remove(0);
    finish(&mut app, next);
    key(&mut app, 'h');
    assert_eq!((app.screen, app.row), (Screen::Slotr, 3));
}
#[test]
fn queue_to_holder_follows_same_enqueue_sequence() {
    let mut app = App::new(frames::fixture());
    key(&mut app, 's');
    app.row = 4;
    let mut next = app.data.slotr.clone();
    let runtime = next.pools.get_mut("runtime").unwrap();
    let mut row = runtime.queue.remove(0);
    row.run = "admitted".into();
    runtime.holders.insert(0, row);
    finish(&mut app, next);
    assert_eq!(app.row, 2);
    assert_eq!(app.data.slotr.pools["runtime"].holders[0].enqueue_seq, 95);
}
