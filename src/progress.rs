use std::env;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Progress {
    enabled: bool,
    color: bool,
    started: Instant,
    message: Arc<Mutex<String>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Progress {
    pub fn new(disabled: bool, color: bool) -> Self {
        let enabled = should_enable(
            disabled,
            io::stderr().is_terminal(),
            env::var("TERM").ok().as_deref(),
            env::var_os("CI").is_some(),
            env::var_os("WORKSTATS_NO_PROGRESS").is_some(),
        );
        let message = Arc::new(Mutex::new("Getting ready".to_string()));
        let stopped = Arc::new(AtomicBool::new(false));
        let started = Instant::now();

        let worker = enabled.then(|| {
            let message = Arc::clone(&message);
            let stopped = Arc::clone(&stopped);
            thread::spawn(move || {
                let mut stderr = io::stderr().lock();
                let mut frame = 0;
                while !stopped.load(Ordering::Relaxed) {
                    let current = message
                        .lock()
                        .map(|value| value.clone())
                        .unwrap_or_else(|_| "Working".to_string());
                    if color {
                        let _ = write!(
                            stderr,
                            "\r\x1b[2K  \x1b[36m{}\x1b[0m {}  \x1b[2m{}\x1b[0m",
                            FRAMES[frame % FRAMES.len()],
                            current,
                            elapsed(started.elapsed())
                        );
                    } else {
                        let _ = write!(
                            stderr,
                            "\r\x1b[2K  {} {}  {}",
                            FRAMES[frame % FRAMES.len()],
                            current,
                            elapsed(started.elapsed())
                        );
                    }
                    let _ = stderr.flush();
                    frame += 1;
                    thread::park_timeout(Duration::from_millis(80));
                }
            })
        });

        Self {
            enabled,
            color,
            started,
            message,
            stopped,
            worker,
        }
    }

    pub fn set(&self, message: impl Into<String>) {
        if let Ok(mut current) = self.message.lock() {
            *current = message.into();
        }
    }

    pub fn finish(mut self, summary: impl AsRef<str>) {
        if !self.enabled {
            return;
        }
        self.stop_worker();
        let mut stderr = io::stderr().lock();
        let mark = if self.color {
            "\x1b[32m✓\x1b[0m"
        } else {
            "✓"
        };
        let subdued = if self.color { "\x1b[2m" } else { "" };
        let reset = if self.color { "\x1b[0m" } else { "" };
        let _ = writeln!(
            stderr,
            "\r\x1b[2K  {mark} {}  {subdued}{}{reset}",
            summary.as_ref(),
            elapsed(self.started.elapsed())
        );
    }

    fn stop_worker(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if !self.enabled || self.worker.is_none() {
            return;
        }
        self.stop_worker();
        let mut stderr = io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[2K");
        let _ = stderr.flush();
    }
}

fn should_enable(
    disabled: bool,
    stderr_is_terminal: bool,
    term: Option<&str>,
    in_ci: bool,
    environment_disabled: bool,
) -> bool {
    !disabled
        && stderr_is_terminal
        && !term.is_some_and(|value| value.eq_ignore_ascii_case("dumb"))
        && !in_ci
        && !environment_disabled
}

fn elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{}s", duration.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_only_animates_in_a_suitable_interactive_terminal() {
        assert!(should_enable(
            false,
            true,
            Some("xterm-256color"),
            false,
            false
        ));
        assert!(!should_enable(
            true,
            true,
            Some("xterm-256color"),
            false,
            false
        ));
        assert!(!should_enable(
            false,
            false,
            Some("xterm-256color"),
            false,
            false
        ));
        assert!(!should_enable(false, true, Some("dumb"), false, false));
        assert!(!should_enable(false, true, Some("xterm"), true, false));
        assert!(!should_enable(false, true, Some("xterm"), false, true));
    }

    #[test]
    fn elapsed_time_is_compact() {
        assert_eq!("1.2s", elapsed(Duration::from_millis(1_234)));
        assert_eq!("12s", elapsed(Duration::from_millis(12_345)));
    }
}
