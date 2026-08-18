//! The `workstats ui` explorer.
//!
//! Everything shown here comes from a report that has ALREADY been built, so
//! the explorer adds no work to a normal run and stdout stays machine-readable
//! for `--format json|csv`.
//!
//! One exception is deliberate and contained: the diff level shells out to
//! `git show` and is the only place in the tool that ever reads file contents.
//! That output is display-only — it is never cached, never written into a
//! report, never stored in a saved view, and never sent anywhere. It lives in
//! memory only while the diff is on screen and is dropped on the way back up.

mod app;
mod diff;
mod event;
mod search;
mod state;
mod views;

use std::env;
use std::io::{self, IsTerminal};

use anyhow::{Context, Result, bail};
use ratatui::DefaultTerminal;

use crate::model::{GitCommit, Report};
use app::App;

pub fn run(report: &Report, commits: Vec<GitCommit>) -> Result<()> {
    if !interactive(io::stdout().is_terminal(), env::var("TERM").ok().as_deref()) {
        bail!(
            "`workstats ui` needs an interactive terminal on stdout; use `--format json` or `--format csv` when redirecting or piping output"
        );
    }
    // Built before the terminal is taken over, so a failure here still reports
    // itself on a normal screen.
    let mut app = App::new(report, commits);
    let mut guard = TerminalGuard::open()?;
    event::run(guard.terminal(), &mut app)
}

/// The same judgement `src/progress.rs` makes before it animates: a terminal
/// that cannot address the cursor is sent no escape codes at all.
fn interactive(stdout_is_terminal: bool, term: Option<&str>) -> bool {
    stdout_is_terminal && !term.is_some_and(|value| value.eq_ignore_ascii_case("dumb"))
}

/// Owns the raw-mode terminal and hands it back from `Drop`, so an error
/// returned out of the event loop — or a panic unwinding through it — cannot
/// leave a shell in raw mode on the alternate screen. Restoring only at the end
/// of the happy path is how a TUI ruins someone's session.
struct TerminalGuard {
    terminal: DefaultTerminal,
}

impl TerminalGuard {
    fn open() -> Result<Self> {
        // `try_init` also installs a panic hook that restores the terminal
        // before the panic is printed, which is what makes the message legible.
        let terminal = ratatui::try_init().context("cannot start the terminal UI")?;
        Ok(Self { terminal })
    }

    fn terminal(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Leaves raw mode and the alternate screen; the terminal dropped just
        // afterwards puts the cursor back on the screen the user came from.
        ratatui::restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_redirected_or_dumb_terminal_gets_no_escape_codes() {
        assert!(interactive(true, Some("xterm-256color")));
        assert!(interactive(true, None));
        assert!(!interactive(false, Some("xterm-256color")));
        assert!(!interactive(true, Some("dumb")));
        assert!(!interactive(true, Some("DUMB")));
    }
}
