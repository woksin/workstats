//! The key map and the loop that drives it.
//!
//! `translate` is deliberately a pure function of the mode and the key: it is
//! the only description of the key map that can be tested without a terminal.

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, read};
use ratatui::DefaultTerminal;

use super::app::App;
use super::state::Mode;
use super::views;

/// What a keystroke means, once the mode is taken into account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Nothing,
    Quit,
    Descend,
    Ascend,
    /// Move the selection by this many rows.
    Move(isize),
    /// Move the selection by this many screens.
    Page(isize),
    First,
    Last,
    SortColumn(usize),
    SortNext,
    SortPrevious,
    ToggleSortOrder,
    ToggleGrain,
    ToggleHelp,
    BeginFilter,
    BeginSearch,
    BeginSaveView,
    OpenViews,
    DeleteView,
    Input(char),
    Backspace,
    Accept,
    Cancel,
}

pub fn translate(mode: Mode, key: KeyEvent) -> Action {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    // Ctrl-C means the same thing everywhere, including halfway through typing
    // a filter, because in raw mode nothing else will deliver it.
    if control && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    match mode {
        Mode::Normal => browsing(key, control),
        Mode::Views => picking(key),
        Mode::Filter | Mode::Search | Mode::SaveView => typing(key, control),
    }
}

fn browsing(key: KeyEvent, control: bool) -> Action {
    match key.code {
        KeyCode::Char('b') if control => Action::Page(-1),
        KeyCode::Char('f') if control => Action::Page(1),
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => Action::Descend,
        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => Action::Ascend,
        KeyCode::Esc => Action::Cancel,
        KeyCode::Up | KeyCode::Char('k') => Action::Move(-1),
        KeyCode::Down | KeyCode::Char('j') => Action::Move(1),
        KeyCode::PageUp => Action::Page(-1),
        KeyCode::PageDown | KeyCode::Char(' ') => Action::Page(1),
        KeyCode::Home | KeyCode::Char('g') => Action::First,
        KeyCode::End | KeyCode::Char('G') => Action::Last,
        KeyCode::Char('/') => Action::BeginFilter,
        KeyCode::Char('s') => Action::BeginSearch,
        KeyCode::Char('[') => Action::SortPrevious,
        KeyCode::Char(']') => Action::SortNext,
        KeyCode::Char('o') => Action::ToggleSortOrder,
        KeyCode::Char('p') => Action::ToggleGrain,
        KeyCode::Char('w') => Action::BeginSaveView,
        KeyCode::Char('v') => Action::OpenViews,
        KeyCode::Char('?') => Action::ToggleHelp,
        KeyCode::Char(digit @ '1'..='9') => Action::SortColumn(digit as usize - '1' as usize),
        _ => Action::Nothing,
    }
}

fn picking(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => Action::Cancel,
        KeyCode::Enter => Action::Accept,
        KeyCode::Up | KeyCode::Char('k') => Action::Move(-1),
        KeyCode::Down | KeyCode::Char('j') => Action::Move(1),
        KeyCode::PageUp => Action::Page(-1),
        KeyCode::PageDown => Action::Page(1),
        KeyCode::Char('d') | KeyCode::Delete => Action::DeleteView,
        KeyCode::Char('q') => Action::Quit,
        _ => Action::Nothing,
    }
}

fn typing(key: KeyEvent, control: bool) -> Action {
    match key.code {
        KeyCode::Esc => Action::Cancel,
        KeyCode::Enter => Action::Accept,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Up => Action::Move(-1),
        KeyCode::Down => Action::Move(1),
        KeyCode::PageUp => Action::Page(-1),
        KeyCode::PageDown => Action::Page(1),
        // Every printable key belongs in the buffer, so only a control chord
        // is allowed to mean something else while text is being entered.
        KeyCode::Char(character) if !control => Action::Input(character),
        _ => Action::Nothing,
    }
}

/// Blocks on the next event rather than polling: nothing on screen animates, so
/// an idle explorer should cost no CPU at all. A resize arrives as an event and
/// the next draw picks up the new size.
pub fn run(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.should_quit() {
        terminal.draw(|frame| views::draw(frame, app))?;
        if let Event::Key(key) = read()?
            && key.kind == KeyEventKind::Press
        {
            let action = translate(app.mode(), key);
            app.apply(action);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn browsing_keys_follow_both_the_arrows_and_the_vi_letters() {
        assert_eq!(Action::Move(1), translate(Mode::Normal, key(KeyCode::Down)));
        assert_eq!(
            Action::Move(1),
            translate(Mode::Normal, key(KeyCode::Char('j')))
        );
        assert_eq!(
            Action::Move(-1),
            translate(Mode::Normal, key(KeyCode::Char('k')))
        );
        assert_eq!(
            Action::Descend,
            translate(Mode::Normal, key(KeyCode::Enter))
        );
        assert_eq!(
            Action::Ascend,
            translate(Mode::Normal, key(KeyCode::Backspace))
        );
        assert_eq!(Action::Cancel, translate(Mode::Normal, key(KeyCode::Esc)));
        assert_eq!(
            Action::ToggleHelp,
            translate(Mode::Normal, key(KeyCode::Char('?')))
        );
    }

    #[test]
    fn a_digit_selects_that_column_counting_from_one() {
        assert_eq!(
            Action::SortColumn(0),
            translate(Mode::Normal, key(KeyCode::Char('1')))
        );
        assert_eq!(
            Action::SortColumn(8),
            translate(Mode::Normal, key(KeyCode::Char('9')))
        );
        // Zero is not a column, so it must not become a sort key.
        assert_eq!(
            Action::Nothing,
            translate(Mode::Normal, key(KeyCode::Char('0')))
        );
    }

    #[test]
    fn typing_swallows_the_letters_that_are_commands_elsewhere() {
        for mode in [Mode::Filter, Mode::Search, Mode::SaveView] {
            assert_eq!(Action::Input('q'), translate(mode, key(KeyCode::Char('q'))));
            assert_eq!(Action::Input('/'), translate(mode, key(KeyCode::Char('/'))));
            assert_eq!(Action::Accept, translate(mode, key(KeyCode::Enter)));
            assert_eq!(Action::Cancel, translate(mode, key(KeyCode::Esc)));
            assert_eq!(Action::Backspace, translate(mode, key(KeyCode::Backspace)));
        }
    }

    #[test]
    fn control_c_quits_from_every_mode() {
        let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        for mode in [
            Mode::Normal,
            Mode::Filter,
            Mode::Search,
            Mode::SaveView,
            Mode::Views,
        ] {
            assert_eq!(Action::Quit, translate(mode, interrupt));
        }
    }

    #[test]
    fn the_view_picker_has_its_own_short_key_map() {
        assert_eq!(
            Action::DeleteView,
            translate(Mode::Views, key(KeyCode::Char('d')))
        );
        assert_eq!(Action::Accept, translate(Mode::Views, key(KeyCode::Enter)));
        assert_eq!(Action::Move(-1), translate(Mode::Views, key(KeyCode::Up)));
        // The picker does not descend, filter, or sort.
        assert_eq!(
            Action::Nothing,
            translate(Mode::Views, key(KeyCode::Char('/')))
        );
    }
}
