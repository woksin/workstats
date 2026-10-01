//! `workstats now`: today and the week so far in one short line, cheap enough
//! for a prompt or a status bar.
//!
//! A prompt runs this on every redraw, so the cost that matters is the one
//! paid when nothing has changed. The last result is kept in `now.json` beside
//! the index, and a snapshot that is still fresh (same flags, same local date,
//! younger than `--max-age`) is printed straight from the file: no scan, no
//! pricing, and the only things read before that decision are the flags, the
//! small config file and the snapshot itself.
//!
//! A stale snapshot is recomputed over the ISO week so far (plus the month so
//! far, when something needs it) through the same pipeline a report uses, so
//! the line cannot disagree with `workstats --week current`. With `--no-wait`
//! the stale line is printed at once and a detached copy of this command
//! refreshes the file for the next call; `now.lock` keeps a slow refresh from
//! piling up copies of itself.
//!
//! The snapshot holds figures and the active session's repository label and
//! branch, never a prompt, a path or a session id (see docs/privacy.md).

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration as StdDuration, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Local, NaiveDate, Utc};
use clap::Args;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::{OutputFormat, ReportArguments, duration_flag};
use crate::goals::{self, Goals, NowStatus};
use crate::model::{DayFigures, Diagnostics, Session};
use crate::output::safe_text;
use crate::paths::{default_cache_path, default_config_path};
use crate::pricing::RateOverrides;
use crate::report::{self, Purpose};
use crate::timeutil::parse_duration;

/// Bumped when the snapshot's shape changes; an older file is then ignored
/// and recomputed instead of misread.
const SNAPSHOT_VERSION: u32 = 1;
const DEFAULT_MAX_AGE: &str = "60s";
const DEFAULT_ACTIVE_WITHIN: &str = "10m";
const DEFAULT_TEMPLATE: &str = "{human} · {agent} agent · ${value}{warn}";
/// A refresh that has held `now.lock` longer than this is taken to have died.
const LOCK_STALE_AFTER: StdDuration = StdDuration::from_secs(120);

#[derive(Debug, Args)]
pub(crate) struct NowArguments {
    #[command(flatten)]
    pub(crate) report: ReportArguments,
    #[arg(
        long,
        value_name = "STR",
        help = "Output template, for example \"{human} · {agent} agent\" (config: \"now.template\")"
    )]
    pub(crate) template: Option<String>,
    #[arg(
        long,
        value_name = "DUR",
        help = "Reuse the last result if it is younger than this; 0 always recomputes (default: 60s; config: \"now.max_age\")"
    )]
    pub(crate) max_age: Option<String>,
    #[arg(
        long,
        help = "Print the last result at once and refresh in the background"
    )]
    pub(crate) no_wait: bool,
    #[arg(
        long,
        value_name = "DUR",
        help = "How recent a session must be to count as active (default: 10m; config: \"now.active_within\")"
    )]
    pub(crate) active_within: Option<String>,
    #[arg(long, help = "Print nothing and exit 0 when it fails, for prompts")]
    pub(crate) quiet_errors: bool,
    #[arg(long, hide = true)]
    pub(crate) refresh_only: bool,
}

/// The config's `now` block.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct NowConfig {
    template: Option<String>,
    max_age: Option<String>,
    active_within: Option<String>,
}

impl NowConfig {
    fn from_config(value: Option<&serde_json::Value>) -> Result<Self> {
        match value.filter(|value| !value.is_null()) {
            None => Ok(Self::default()),
            Some(value) => serde_json::from_value(value.clone()).context(
                "invalid \"now\" configuration (expected template, max_age, active_within)",
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Template
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Token {
    Human,
    HumanDecimal,
    Agent,
    Prompts,
    Commits,
    Sessions,
    Active,
    ActiveProvider,
    ActiveRepo,
    ActiveBranch,
    Value,
    ValueWeek,
    ValueMonth,
    WeekHuman,
    WeekTarget,
    WeekPct,
    CapPct,
    Warn,
    Stale,
}

const TOKENS: [(&str, Token); 19] = [
    ("human", Token::Human),
    ("human_decimal", Token::HumanDecimal),
    ("agent", Token::Agent),
    ("prompts", Token::Prompts),
    ("commits", Token::Commits),
    ("sessions", Token::Sessions),
    ("active", Token::Active),
    ("active_provider", Token::ActiveProvider),
    ("active_repo", Token::ActiveRepo),
    ("active_branch", Token::ActiveBranch),
    ("value", Token::Value),
    ("value_week", Token::ValueWeek),
    ("value_month", Token::ValueMonth),
    ("week_human", Token::WeekHuman),
    ("week_target", Token::WeekTarget),
    ("week_pct", Token::WeekPct),
    ("cap_pct", Token::CapPct),
    ("warn", Token::Warn),
    ("stale", Token::Stale),
];

#[derive(Clone, Debug, PartialEq)]
enum Piece {
    Text(String),
    Token(Token),
}

/// A parsed output template. `{name}` is a figure, `{{` and `}}` are literal
/// braces, and a name that is not one of [`TOKENS`] is an error: a typo that
/// printed nothing would look exactly like a quiet day.
#[derive(Clone, Debug, PartialEq)]
struct Template {
    pieces: Vec<Piece>,
}

impl Template {
    fn parse(text: &str) -> Result<Self> {
        let mut pieces = Vec::new();
        let mut literal = String::new();
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            match character {
                '{' if characters.peek() == Some(&'{') => {
                    characters.next();
                    literal.push('{');
                }
                '}' if characters.peek() == Some(&'}') => {
                    characters.next();
                    literal.push('}');
                }
                '{' => {
                    let mut name = String::new();
                    loop {
                        match characters.next() {
                            Some('}') => break,
                            Some(next) if next.is_ascii_lowercase() || next == '_' => {
                                name.push(next);
                            }
                            Some(next) => bail!(
                                "unexpected {next:?} inside a template token; tokens are {}",
                                known_tokens()
                            ),
                            None => bail!(
                                "unclosed '{{' in the template; write '{{{{' for a literal brace"
                            ),
                        }
                    }
                    let Some((_, token)) = TOKENS.iter().find(|(known, _)| *known == name) else {
                        bail!(
                            "unknown template token {{{name}}}; tokens are {}",
                            known_tokens()
                        );
                    };
                    if !literal.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut literal)));
                    }
                    pieces.push(Piece::Token(*token));
                }
                '}' => bail!("unmatched '}}' in the template; write '}}}}' for a literal brace"),
                other => literal.push(other),
            }
        }
        if !literal.is_empty() {
            pieces.push(Piece::Text(literal));
        }
        Ok(Self { pieces })
    }

    fn uses(&self, token: Token) -> bool {
        self.pieces.contains(&Piece::Token(token))
    }
}

fn known_tokens() -> String {
    TOKENS
        .iter()
        .map(|(name, _)| format!("{{{name}}}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Today's figures, the week's, and the list value for today, the week and
/// (when it was computed) the month.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct Figures {
    human_seconds: f64,
    agent_seconds: f64,
    prompts: usize,
    commits: usize,
    sessions: usize,
    week_human_seconds: f64,
    value_usd: f64,
    value_week_usd: f64,
    /// `None` when nothing asked for the month, so it was not looked up.
    value_month_usd: Option<f64>,
    /// Token events with no list rate, left out of every value above.
    unpriced_events: usize,
}

/// The newest foreground session in the window. Whether it still counts as
/// active depends on `--active-within` and the clock at print time, so the
/// snapshot keeps when it was last seen rather than a verdict.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct Active {
    provider: String,
    repo: String,
    branch: Option<String>,
    last_seen: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct Snapshot {
    version: u32,
    args_hash: String,
    computed_at: DateTime<Utc>,
    local_date: NaiveDate,
    figures: Figures,
    active: Option<Active>,
    #[serde(default)]
    goals: NowStatus,
}

/// Where the snapshot lives: `WORKSTATS_NOW_CACHE`, else `now.json` beside the
/// index, which `--cache` and `WORKSTATS_CACHE` move.
fn snapshot_path(cache: Option<&Path>) -> PathBuf {
    if let Some(path) = env::var_os("WORKSTATS_NOW_CACHE") {
        return PathBuf::from(path);
    }
    let index = cache.map_or_else(default_cache_path, Path::to_path_buf);
    index.parent().map_or_else(
        || PathBuf::from("now.json"),
        |directory| directory.join("now.json"),
    )
}

fn lock_path(snapshot: &Path) -> PathBuf {
    snapshot.with_extension("lock")
}

/// Deletes the snapshot, because `--rebuild-cache` means "distrust what was
/// stored". A missing file is the normal case.
pub(crate) fn remove_snapshot(cache: Option<&Path>, diagnostics: &mut Diagnostics) {
    if let Err(error) = remove_file(&snapshot_path(cache)) {
        diagnostics.warn(format!("now snapshot not removed: {error:#}"));
    }
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("cannot remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// The snapshot, or `None` when there is none or it is not one this version
/// wrote. A cache that cannot be read is simply a cache miss.
fn read_snapshot(path: &Path) -> Option<Snapshot> {
    let bytes = fs::read(path).ok()?;
    let snapshot: Snapshot = serde_json::from_slice(&bytes).ok()?;
    (snapshot.version == SNAPSHOT_VERSION).then_some(snapshot)
}

/// Written next to its destination and renamed into place, so a prompt reading
/// while a refresh writes sees the old file or the new one, never half of one.
fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .with_context(|| format!("cannot write beside {}", path.display()))?;
    serde_json::to_writer(&mut file, snapshot)?;
    file.flush()?;
    file.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// A hash of everything that decides the figures other than the clock: the
/// flags, the working directory and environment the default root comes from,
/// and the config file (path, size and modification time), so editing the
/// config invalidates the snapshot without reading it twice.
fn args_hash(
    report: &ReportArguments,
    config_path: &Path,
    config_fingerprint: &str,
    needs_month: bool,
) -> String {
    let mut parts = vec![
        format!("workstats {}", env!("CARGO_PKG_VERSION")),
        format!("{report:?}"),
        format!("config {} {config_fingerprint}", config_path.display()),
        format!("month {needs_month}"),
        format!("cwd {:?}", env::current_dir().ok()),
    ];
    for name in [
        "WORKSTATS_AUTHOR",
        "WORKSTATS_DIR",
        "WORKSTATS_GIT",
        "WORKSTATS_EVENTS",
    ] {
        parts.push(format!("{name}={:?}", env::var_os(name)));
    }
    let digest = Sha256::digest(parts.join("\n").as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Size and modification time of the config file, or `none`.
fn config_fingerprint(path: &Path) -> String {
    let Ok(metadata) = fs::metadata(path) else {
        return "none".to_string();
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{}:{modified}", metadata.len())
}

fn local_today(now: DateTime<Utc>) -> NaiveDate {
    now.with_timezone(&Local).date_naive()
}

fn local_day(at: DateTime<Utc>) -> NaiveDate {
    local_today(at)
}

/// What was decided about the snapshot, and so what is printed.
#[derive(Debug, PartialEq)]
enum Served {
    /// Young enough: printed as it is, nothing computed.
    Fresh(Snapshot),
    /// Old, but printed at once under `--no-wait` while a refresh runs.
    Stale(Snapshot),
    /// Computed just now.
    Computed(Snapshot),
}

fn is_fresh(snapshot: &Snapshot, hash: &str, now: DateTime<Utc>, max_age: Duration) -> bool {
    let age = now - snapshot.computed_at;
    snapshot.args_hash == hash
        && snapshot.local_date == local_today(now)
        // A snapshot from the future (a clock that moved back) proves nothing.
        && age >= Duration::zero()
        && age < max_age
}

/// The decision at the heart of the command, with the clock, the computation
/// and the refresh passed in so it can be tested without a scan.
///
/// Only a snapshot from this local day and these flags is ever shown stale:
/// yesterday's "today" would be wrong, not merely old, so in that case the
/// first call of the day waits for a real answer.
fn serve(
    existing: Option<Snapshot>,
    hash: &str,
    now: DateTime<Utc>,
    max_age: Duration,
    no_wait: bool,
    compute: impl FnOnce() -> Result<Snapshot>,
    refresh: impl FnOnce(),
) -> Result<Served> {
    if let Some(snapshot) = existing {
        if is_fresh(&snapshot, hash, now, max_age) {
            return Ok(Served::Fresh(snapshot));
        }
        if no_wait && snapshot.args_hash == hash && snapshot.local_date == local_today(now) {
            refresh();
            return Ok(Served::Stale(snapshot));
        }
    }
    compute().map(Served::Computed)
}

// ---------------------------------------------------------------------------
// Lock
// ---------------------------------------------------------------------------

/// Held by the one background refresh at a time. Created with `create_new`, so
/// two refreshes cannot both win; one older than [`LOCK_STALE_AFTER`] belongs
/// to a refresh that died and is taken over. (Two processes taking over the
/// same stale lock in the same instant can both proceed; the cost is one
/// redundant refresh, and the snapshot is replaced atomically either way.)
struct RefreshLock {
    path: PathBuf,
}

impl RefreshLock {
    /// `Ok(None)` when a live refresh holds the lock.
    fn acquire(path: &Path, now: SystemTime) -> io::Result<Option<Self>> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    // The pid only helps a person looking at a stuck lock.
                    let _ = write!(file, "{}", std::process::id());
                    return Ok(Some(Self {
                        path: path.to_path_buf(),
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if lock_is_live(path, now) {
                        return Ok(None);
                    }
                    match fs::remove_file(path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A lock younger than [`LOCK_STALE_AFTER`]. One whose age cannot be read is
/// not live, so it cannot wedge the refresh forever.
fn lock_is_live(path: &Path, now: SystemTime) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .is_some_and(|modified| {
            now.duration_since(modified)
                .map_or(true, |age| age < LOCK_STALE_AFTER)
        })
}

/// Starts a detached copy of this command that only refreshes the snapshot.
///
/// The arguments are this process's own, minus `--no-wait` and
/// `--rebuild-cache` (the first would loop, the second would rebuild the index
/// a second time) and plus `--refresh-only`, so the copy computes exactly what
/// the caller asked for. It does not wait for the child: the caller is a
/// prompt and has to return now.
// The child outlives this process on purpose, so there is nothing to wait for.
#[allow(clippy::zombie_processes)]
fn spawn_refresh(lock: &Path) -> Result<()> {
    if lock_is_live(lock, SystemTime::now()) {
        return Ok(());
    }
    let mut arguments: Vec<OsString> = env::args_os()
        .skip(1)
        .filter(|argument| argument != "--no-wait" && argument != "--rebuild-cache")
        .collect();
    arguments.push("--refresh-only".into());
    let mut command = Command::new(env::current_exe().context("cannot find this executable")?);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so the shell's Ctrl-C and the terminal closing
        // do not take the refresh down with the prompt that started it.
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NO_WINDOW
        command.creation_flags(0x0000_0008 | 0x0800_0000);
    }
    command
        .spawn()
        .context("cannot start the background refresh")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Computing
// ---------------------------------------------------------------------------

struct SnapshotInputs<'a> {
    daily: &'a [DayFigures],
    sessions: &'a [Session],
    overrides: &'a RateOverrides,
    goals: Option<&'a Goals>,
    needs_month: bool,
    now: DateTime<Utc>,
    args_hash: &'a str,
}

/// Today's and the week's figures from a collected run.
///
/// Everything is read off the daily figures and the sessions' token events the
/// report was built from, so a number here is the number `--daily` would show.
fn build_snapshot(inputs: &SnapshotInputs<'_>) -> Snapshot {
    let SnapshotInputs {
        daily,
        sessions,
        overrides,
        goals,
        needs_month,
        now,
        args_hash,
    } = inputs;
    let today = local_today(*now);
    let monday = goals::week_start(today);
    let month_first = goals::month_start(today);

    let figures_today = daily.iter().find(|day| day.date == today);
    let week_human_seconds: f64 = daily
        .iter()
        .filter(|day| day.date >= monday && day.date <= today)
        .map(|day| day.human_seconds)
        .sum();

    let priced = goals::price_events(sessions, overrides);
    let value_since = |from: NaiveDate| -> f64 {
        priced
            .events
            .iter()
            .filter(|event| local_day(event.at) >= from)
            .map(|event| event.usd)
            .sum()
    };

    let active = sessions
        .iter()
        .filter(|session| !session.is_subagent)
        .filter_map(|session| session.last_seen().map(|seen| (seen, session)))
        .max_by_key(|(seen, _)| *seen)
        .map(|(seen, session)| Active {
            provider: session.provider.clone(),
            repo: session.repo.clone(),
            branch: session.branch_at(seen).map(str::to_string),
            last_seen: seen,
        });

    let goals_status = goals.map(|goals| {
        goals::now_status(
            goals,
            week_human_seconds,
            &goals::pool_totals(&priced, monday),
            &goals::pool_totals(&priced, month_first),
        )
    });

    Snapshot {
        version: SNAPSHOT_VERSION,
        args_hash: (*args_hash).to_string(),
        computed_at: *now,
        local_date: today,
        figures: Figures {
            human_seconds: figures_today.map_or(0.0, |day| day.human_seconds),
            agent_seconds: figures_today.map_or(0.0, |day| day.agent_wall_seconds),
            prompts: figures_today.map_or(0, |day| day.prompts),
            commits: figures_today.map_or(0, |day| day.commits),
            sessions: figures_today.map_or(0, |day| day.sessions),
            week_human_seconds,
            value_usd: value_since(today),
            value_week_usd: value_since(monday),
            value_month_usd: needs_month.then(|| value_since(month_first)),
            unpriced_events: priced.unpriced,
        },
        active,
        goals: goals_status.unwrap_or_default(),
    }
}

struct Settings {
    goals: Option<Goals>,
    needs_month: bool,
}

/// Collects the week so far (and the month so far when something needs it)
/// through the report pipeline and boils it down to a snapshot.
fn compute(
    mut report: ReportArguments,
    settings: &Settings,
    args_hash: &str,
    now: DateTime<Utc>,
) -> Result<Snapshot> {
    let today = local_today(now);
    let mut first = goals::week_start(today);
    if settings.needs_month {
        first = first.min(goals::month_start(today));
    }
    // `--since` takes a local date, so this is the start of that day. The
    // window is open at the end: nothing in the future is in the history.
    report.since = Some(first.to_string());
    report.daily = true;
    // A prompt must not draw progress, check for updates, or compute goals
    // twice: the status is derived below from the same sessions.
    report.no_progress = true;
    report.no_update_check = true;
    report.no_goals = true;
    let collected = report::collect(report, Purpose::Query)?;
    let daily = collected.report.daily.as_deref().unwrap_or_default();
    Ok(build_snapshot(&SnapshotInputs {
        daily,
        sessions: &collected.sessions,
        overrides: &collected.settings.rate_overrides,
        goals: settings.goals.as_ref(),
        needs_month: settings.needs_month,
        now,
        args_hash,
    }))
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// `2h05m`, or `45m` under an hour.
fn compact_duration(seconds: f64) -> String {
    let minutes = (seconds / 60.0).round().max(0.0) as u64;
    if minutes >= 60 {
        format!("{}h{:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

fn usd(value: f64) -> String {
    // An empty sum of f64 is negative zero, which would print as "-0.00".
    format!("{:.2}", value + 0.0)
}

fn percent(fraction: f64) -> String {
    format!("{:.0}%", fraction * 100.0)
}

/// The session counts as active when it was seen within `within` of `now`. A
/// session stamped slightly ahead of this clock is active, not unknown.
fn active_session(snapshot: &Snapshot, now: DateTime<Utc>, within: Duration) -> Option<&Active> {
    snapshot
        .active
        .as_ref()
        .filter(|active| now - active.last_seen <= within)
}

fn token_text(token: Token, snapshot: &Snapshot, stale: bool, active: Option<&Active>) -> String {
    let figures = &snapshot.figures;
    let goals = &snapshot.goals;
    // Text from a session is not text this tool wrote, and a prompt is a
    // terminal: control characters are replaced the way every table does.
    let text = |value: &str| safe_text(value);
    match token {
        Token::Human => compact_duration(figures.human_seconds),
        Token::HumanDecimal => format!("{:.1}", figures.human_seconds / 3600.0),
        Token::Agent => compact_duration(figures.agent_seconds),
        Token::Prompts => figures.prompts.to_string(),
        Token::Commits => figures.commits.to_string(),
        Token::Sessions => figures.sessions.to_string(),
        Token::Active => active.map_or(String::new(), |_| "●".to_string()),
        Token::ActiveProvider => active.map_or(String::new(), |a| text(&a.provider)),
        Token::ActiveRepo => active.map_or(String::new(), |a| text(&a.repo)),
        Token::ActiveBranch => active
            .and_then(|a| a.branch.as_deref())
            .map_or(String::new(), text),
        Token::Value => usd(figures.value_usd),
        Token::ValueWeek => usd(figures.value_week_usd),
        Token::ValueMonth => figures
            .value_month_usd
            .map_or_else(|| "n/a".to_string(), usd),
        Token::WeekHuman => compact_duration(figures.week_human_seconds),
        Token::WeekTarget => goals
            .week_target_hours
            .map_or(String::new(), goals::trimmed),
        Token::WeekPct => goals.week_fraction.map_or(String::new(), percent),
        Token::CapPct => goals.cap_fraction.map_or(String::new(), percent),
        // A leading space, so the default template ends clean when nothing is
        // wrong and a custom one can put `{warn}` flush against what precedes.
        Token::Warn => {
            if goals.warnings.is_empty() {
                String::new()
            } else {
                format!(" {}", goals.warnings.join(" "))
            }
        }
        Token::Stale => {
            if stale {
                "~".to_string()
            } else {
                String::new()
            }
        }
    }
}

fn render_line(
    template: &Template,
    snapshot: &Snapshot,
    stale: bool,
    active: Option<&Active>,
) -> String {
    let mut line = String::new();
    for piece in &template.pieces {
        match piece {
            Piece::Text(text) => line.push_str(text),
            Piece::Token(token) => line.push_str(&token_text(*token, snapshot, stale, active)),
        }
    }
    line.trim_end().to_string()
}

#[derive(Serialize)]
struct NowOutput<'a> {
    stale: bool,
    computed_at: DateTime<Utc>,
    local_date: NaiveDate,
    #[serde(flatten)]
    figures: &'a Figures,
    active: Option<&'a Active>,
    goals: &'a NowStatus,
}

fn render_json(snapshot: &Snapshot, stale: bool, active: Option<&Active>) -> Result<String> {
    Ok(serde_json::to_string_pretty(&NowOutput {
        stale,
        computed_at: snapshot.computed_at,
        local_date: snapshot.local_date,
        figures: &snapshot.figures,
        active,
        goals: &snapshot.goals,
    })?)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The window is always the current week (and month), so the flags that pick
/// another one are refused rather than ignored.
fn refuse_window_flags(report: &ReportArguments) -> Result<()> {
    let given = [
        ("--since", report.since.is_some()),
        ("--until", report.until.is_some()),
        ("--month", report.month.is_some()),
        ("--year", report.year.is_some()),
        ("--week", report.week.is_some()),
        ("--compare", report.compare.is_some()),
    ];
    if let Some((flag, _)) = given.iter().find(|(_, set)| *set) {
        bail!(
            "{flag} is not available for `workstats now`, which always reports today and the current week; use `workstats --week current --daily` for another window"
        );
    }
    Ok(())
}

/// Reads the config once: its `now` block, its `goals` block and the
/// fingerprint that goes into the hash. A file that is not JSON is left to
/// the report pipeline to warn about; here it just contributes nothing.
fn read_config(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// An age of zero ("0", "0s") always recomputes, which `parse_duration`
/// refuses everywhere else.
fn parse_age(value: &str) -> Result<Duration> {
    let trimmed = value.trim();
    let digits = trimmed.trim_end_matches(['s', 'm', 'h', 'S', 'M', 'H']);
    if !digits.is_empty()
        && digits.chars().all(|c| c == '0' || c == '.')
        && digits.chars().any(|c| c == '0')
    {
        return Ok(Duration::zero());
    }
    parse_duration(value).with_context(|| format!("invalid --max-age {value:?}"))
}

pub(crate) fn run(arguments: NowArguments) -> Result<()> {
    let quiet = arguments.quiet_errors;
    match run_at(arguments, Utc::now()) {
        Ok(Some(text)) => {
            // A prompt that closed its end early is not an error worth a message.
            let _ = writeln!(io::stdout(), "{text}");
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(_) if quiet => Ok(()),
        Err(error) => Err(error),
    }
}

fn run_at(arguments: NowArguments, now: DateTime<Utc>) -> Result<Option<String>> {
    let NowArguments {
        report,
        template,
        max_age,
        no_wait,
        active_within,
        quiet_errors,
        refresh_only,
    } = arguments;
    refuse_window_flags(&report)?;
    let json = match report.output_format {
        None | Some(OutputFormat::Table) => false,
        Some(OutputFormat::Json) => true,
        Some(other) => bail!(
            "--format {} is not available for `workstats now`; use table or json",
            other.name()
        ),
    };

    // Everything below is what the fast path costs: the config file, the flags
    // and the snapshot. No sources are opened and no index is touched.
    let config_path = report.config.clone().unwrap_or_else(default_config_path);
    let config = read_config(&config_path);
    let block = |name: &str| config.as_ref().and_then(|config| config.get(name));
    let now_config = NowConfig::from_config(block("now"))?;
    let goals = if report.no_goals {
        None
    } else {
        Goals::from_config(block("goals"))?
    };
    let template = Template::parse(
        template
            .as_deref()
            .or(now_config.template.as_deref())
            .unwrap_or(DEFAULT_TEMPLATE),
    )?;
    let max_age = parse_age(
        max_age
            .as_deref()
            .or(now_config.max_age.as_deref())
            .unwrap_or(DEFAULT_MAX_AGE),
    )?;
    let active_within = duration_flag(
        "--active-within",
        active_within
            .as_deref()
            .or(now_config.active_within.as_deref())
            .unwrap_or(DEFAULT_ACTIVE_WITHIN),
    )?;
    let settings = Settings {
        needs_month: json
            || template.uses(Token::ValueMonth)
            || goals.as_ref().is_some_and(Goals::has_month_cap),
        goals,
    };
    let hash = args_hash(
        &report,
        &config_path,
        &config_fingerprint(&config_path),
        settings.needs_month,
    );
    let path = snapshot_path(report.cache.as_deref());
    let lock = lock_path(&path);

    if report.rebuild_cache {
        remove_file(&path)?;
    }

    if refresh_only {
        // The detached copy: compute, store, say nothing. Losing the race for
        // the lock means a refresh is already running, which is the point.
        let Some(_held) = RefreshLock::acquire(&lock, SystemTime::now())? else {
            return Ok(None);
        };
        let snapshot = compute(report, &settings, &hash, now)?;
        write_snapshot(&path, &snapshot)?;
        return Ok(None);
    }

    let served = serve(
        read_snapshot(&path),
        &hash,
        now,
        max_age,
        no_wait,
        || compute(report, &settings, &hash, now),
        || {
            if let Err(error) = spawn_refresh(&lock)
                && !quiet_errors
            {
                eprintln!("workstats: {error:#}");
            }
        },
    )?;
    if let Served::Computed(snapshot) = &served
        && let Err(error) = write_snapshot(&path, snapshot)
        && !quiet_errors
    {
        // The answer is still good; only the next call loses its shortcut.
        eprintln!("workstats: now snapshot not saved: {error:#}");
    }
    let (snapshot, stale) = match &served {
        Served::Fresh(snapshot) | Served::Computed(snapshot) => (snapshot, false),
        Served::Stale(snapshot) => (snapshot, true),
    };
    let active = active_session(snapshot, now, active_within);
    Ok(Some(if json {
        render_json(snapshot, stale, active)?
    } else {
        render_line(&template, snapshot, stale, active)
    }))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use chrono::TimeZone;
    use clap::Parser;

    use super::*;
    use crate::cli::{Arguments, Command};
    use crate::model::{ActivityPoint, BranchMark, BranchSource, TokenEvent, TokenUsage};

    fn parse(flags: &[&str]) -> NowArguments {
        let mut all = vec!["workstats", "now"];
        all.extend_from_slice(flags);
        match Arguments::try_parse_from(all).unwrap().command {
            Some(Command::Now(arguments)) => *arguments,
            other => panic!("not `now`: {other:?}"),
        }
    }

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        // 2026-08-12 is a Wednesday, in ISO week 33 (Monday the 10th).
        Local
            .with_ymd_and_hms(2026, 8, day, hour, minute, 0)
            .single()
            .expect("an unambiguous local time")
            .with_timezone(&Utc)
    }

    fn snapshot(hash: &str, computed_at: DateTime<Utc>) -> Snapshot {
        Snapshot {
            version: SNAPSHOT_VERSION,
            args_hash: hash.to_string(),
            computed_at,
            local_date: local_today(computed_at),
            figures: Figures {
                human_seconds: 2.0 * 3600.0 + 5.0 * 60.0,
                agent_seconds: 90.0 * 60.0,
                prompts: 12,
                commits: 3,
                sessions: 2,
                week_human_seconds: 11.5 * 3600.0,
                value_usd: 4.5,
                value_week_usd: 20.0,
                value_month_usd: Some(80.0),
                unpriced_events: 0,
            },
            active: Some(Active {
                provider: "claude".to_string(),
                repo: "workstats".to_string(),
                branch: Some("feat/now".to_string()),
                last_seen: computed_at - Duration::minutes(3),
            }),
            goals: NowStatus {
                week_target_hours: Some(37.5),
                week_fraction: Some(0.3),
                cap_fraction: Some(0.88),
                warnings: vec!["⚠ claude 88%".to_string()],
            },
        }
    }

    fn never() -> Result<Snapshot> {
        panic!("the snapshot was fresh, so nothing may be computed")
    }

    #[test]
    fn a_fresh_snapshot_is_served_without_computing() {
        let computed = at(12, 10, 0);
        let stored = snapshot("h", computed);
        let served = serve(
            Some(stored.clone()),
            "h",
            computed + Duration::seconds(59),
            Duration::seconds(60),
            false,
            never,
            || panic!("no refresh is needed"),
        )
        .unwrap();
        assert_eq!(Served::Fresh(stored), served);
    }

    #[test]
    fn each_thing_that_invalidates_a_snapshot_forces_a_compute() {
        let computed = at(12, 10, 0);
        let stored = snapshot("h", computed);
        let fresh_now = computed + Duration::seconds(10);
        let calls = Cell::new(0);
        let attempt = |existing: Option<Snapshot>, hash: &str, now: DateTime<Utc>| {
            let fresh = snapshot(hash, now);
            let served = serve(
                existing,
                hash,
                now,
                Duration::seconds(60),
                false,
                || {
                    calls.set(calls.get() + 1);
                    Ok(fresh)
                },
                || panic!("not --no-wait"),
            )
            .unwrap();
            matches!(served, Served::Computed(_))
        };
        // Control: this one is fresh.
        assert!(!attempt(Some(stored.clone()), "h", fresh_now));
        assert_eq!(0, calls.get());
        // Other flags.
        assert!(attempt(Some(stored.clone()), "other", fresh_now));
        // Too old.
        assert!(attempt(
            Some(stored.clone()),
            "h",
            computed + Duration::seconds(60)
        ));
        // Another local day, even a minute old.
        let late = snapshot("h", at(12, 23, 59));
        assert!(attempt(Some(late), "h", at(13, 0, 1)));
        // A snapshot from the future.
        assert!(attempt(
            Some(stored.clone()),
            "h",
            computed - Duration::seconds(5)
        ));
        // No snapshot at all.
        assert!(attempt(None, "h", fresh_now));
        assert_eq!(5, calls.get());
    }

    #[test]
    fn zero_max_age_always_recomputes() {
        let computed = at(12, 10, 0);
        let stored = snapshot("h", computed);
        let age = parse_age("0s").unwrap();
        assert_eq!(Duration::zero(), age);
        assert_eq!(Duration::zero(), parse_age("0").unwrap());
        assert!(!is_fresh(&stored, "h", computed, age));
        assert!(parse_age("0x").is_err());
        assert_eq!(Duration::seconds(90), parse_age("90s").unwrap());
    }

    #[test]
    fn no_wait_serves_the_stale_snapshot_and_starts_one_refresh() {
        let computed = at(12, 10, 0);
        let stored = snapshot("h", computed);
        let refreshed = Cell::new(0);
        let served = serve(
            Some(stored.clone()),
            "h",
            computed + Duration::minutes(30),
            Duration::seconds(60),
            true,
            never,
            || refreshed.set(refreshed.get() + 1),
        )
        .unwrap();
        assert_eq!(Served::Stale(stored), served);
        assert_eq!(1, refreshed.get());
    }

    #[test]
    fn no_wait_never_shows_another_days_or_other_flags_figures() {
        let yesterday = snapshot("h", at(11, 22, 0));
        for (existing, hash) in [(yesterday.clone(), "h"), (snapshot("h", at(12, 9, 0)), "x")] {
            let computed = Cell::new(false);
            let served = serve(
                Some(existing),
                hash,
                at(12, 10, 0),
                Duration::seconds(60),
                true,
                || {
                    computed.set(true);
                    Ok(snapshot(hash, at(12, 10, 0)))
                },
                || panic!("a wrong-day snapshot is not shown, so nothing refreshes behind it"),
            )
            .unwrap();
            assert!(matches!(served, Served::Computed(_)));
            assert!(computed.get());
        }
    }

    #[test]
    fn the_template_substitutes_tokens_and_braces() {
        let computed = at(12, 10, 0);
        let stored = snapshot("h", computed);
        let active = active_session(&stored, computed, Duration::minutes(10));
        let render = |text: &str, stale: bool| {
            render_line(&Template::parse(text).unwrap(), &stored, stale, active)
        };
        assert_eq!(
            "2h05m · 1h30m agent · $4.50 ⚠ claude 88%",
            render(DEFAULT_TEMPLATE, false)
        );
        assert_eq!(
            "● claude workstats feat/now 2.1 12 3 2",
            render(
                "{active} {active_provider} {active_repo} {active_branch} {human_decimal} {prompts} {commits} {sessions}",
                false
            )
        );
        assert_eq!(
            "11h30m of 37.5 (30%) cap 88% $20.00 $80.00",
            render(
                "{week_human} of {week_target} ({week_pct}) cap {cap_pct} ${value_week} ${value_month}",
                false
            )
        );
        assert_eq!("{literal}~", render("{{literal}}{stale}", true));
        assert_eq!("x", render("x{stale}", false));
    }

    #[test]
    fn an_idle_session_is_not_active() {
        let computed = at(12, 10, 0);
        let stored = snapshot("h", computed);
        let template = Template::parse("[{active}{active_repo}{active_branch}]").unwrap();
        // Seen three minutes before the snapshot; ten minutes after being seen is
        // still active, one second more is not.
        let eleven = computed + Duration::minutes(7);
        assert!(active_session(&stored, eleven, Duration::minutes(10)).is_some());
        let later = computed + Duration::minutes(7) + Duration::seconds(1);
        assert!(active_session(&stored, later, Duration::minutes(10)).is_none());
        assert_eq!(
            "[]",
            render_line(
                &template,
                &stored,
                false,
                active_session(&stored, later, Duration::minutes(10))
            )
        );
    }

    #[test]
    fn unknown_tokens_and_broken_braces_are_errors() {
        let error = Template::parse("{human} {humn}").unwrap_err().to_string();
        assert!(error.contains("{humn}"), "{error}");
        assert!(
            error.contains("{value_month}"),
            "the valid tokens are listed: {error}"
        );
        assert!(
            Template::parse("{human")
                .unwrap_err()
                .to_string()
                .contains("unclosed")
        );
        assert!(
            Template::parse("human}")
                .unwrap_err()
                .to_string()
                .contains("unmatched")
        );
        assert!(Template::parse("{Human}").is_err());
        assert!(Template::parse("{ human }").is_err());
        assert!(Template::parse("").unwrap().pieces.is_empty());
        // Every documented token parses.
        for (name, _) in TOKENS {
            assert!(Template::parse(&format!("{{{name}}}")).is_ok(), "{name}");
        }
    }

    #[test]
    fn text_from_a_session_cannot_carry_control_characters_into_a_prompt() {
        let mut stored = snapshot("h", at(12, 10, 0));
        stored.active.as_mut().unwrap().repo = "evil\u{1b}[2Jrepo".to_string();
        let template = Template::parse("{active_repo}").unwrap();
        let line = render_line(&template, &stored, false, stored.active.as_ref());
        assert!(!line.contains('\u{1b}'), "{line:?}");
    }

    #[test]
    fn json_output_carries_the_figures_and_the_stale_flag() {
        let stored = snapshot("h", at(12, 10, 0));
        let text = render_json(&stored, true, stored.active.as_ref()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(true, value["stale"]);
        assert_eq!(12, value["prompts"]);
        assert_eq!(80.0, value["value_month_usd"]);
        assert_eq!("feat/now", value["active"]["branch"]);
        assert_eq!("⚠ claude 88%", value["goals"]["warnings"][0]);
        assert!(value.get("args_hash").is_none());
    }

    #[test]
    fn the_hash_follows_the_flags_and_the_month_need_not_the_template_or_age() {
        let path = Path::new("/nonexistent/config.json");
        let hash =
            |flags: &[&str], month: bool| args_hash(&parse(flags).report, path, "none", month);
        let base = hash(&[], false);
        assert_eq!(base, hash(&[], false));
        assert_ne!(base, hash(&["--no-git"], false));
        assert_ne!(base, hash(&["--provider", "claude"], false));
        assert_ne!(base, hash(&["--no-goals"], false));
        assert_ne!(base, hash(&[], true));
        assert_ne!(base, args_hash(&parse(&[]).report, path, "1:2", false));
        // Presentation and freshness settings are not part of what is computed.
        assert_eq!(
            base,
            hash(
                &[
                    "--template",
                    "{human}",
                    "--max-age",
                    "5m",
                    "--active-within",
                    "1h",
                    "--no-wait",
                    "--quiet-errors"
                ],
                false
            )
        );
    }

    #[test]
    fn window_flags_are_refused() {
        for flag in [
            vec!["--since", "2026-01"],
            vec!["--until", "2026-01"],
            vec!["--month", "current"],
            vec!["--year", "2026"],
            vec!["--week", "current"],
        ] {
            let error = refuse_window_flags(&parse(&flag).report).unwrap_err();
            assert!(error.to_string().contains(flag[0]), "{error}");
        }
        assert!(refuse_window_flags(&parse(&[]).report).is_ok());
    }

    #[test]
    fn the_now_block_is_validated() {
        let ok = serde_json::json!({"template": "{human}", "max_age": "2m", "active_within": "5m"});
        let config = NowConfig::from_config(Some(&ok)).unwrap();
        assert_eq!(Some("2m"), config.max_age.as_deref());
        let bad = serde_json::json!({"templat": "x"});
        assert!(
            NowConfig::from_config(Some(&bad))
                .unwrap_err()
                .to_string()
                .contains("now")
        );
        assert!(NowConfig::from_config(None).unwrap().template.is_none());
    }

    #[test]
    fn a_live_lock_is_respected_and_a_stale_one_is_taken_over() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("now.lock");
        let now = SystemTime::now();

        let first = RefreshLock::acquire(&path, now)
            .unwrap()
            .expect("the first wins");
        assert!(
            RefreshLock::acquire(&path, now).unwrap().is_none(),
            "a second refresh must not start while one holds the lock"
        );
        assert!(
            RefreshLock::acquire(&path, now + StdDuration::from_secs(119))
                .unwrap()
                .is_none(),
            "still live just under two minutes"
        );
        // More than two minutes on, the lock is ignored and replaced.
        let later = now + StdDuration::from_secs(121);
        let second = RefreshLock::acquire(&path, later).unwrap();
        assert!(second.is_some(), "a stale lock is ignored");
        assert!(path.exists());
        // Releasing removes the file, and a new refresh can start at once.
        drop(second);
        drop(first);
        assert!(!path.exists());
        assert!(RefreshLock::acquire(&path, now).unwrap().is_some());
    }

    #[test]
    fn the_snapshot_is_written_atomically_and_read_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("now.json");
        assert!(read_snapshot(&path).is_none());
        let stored = snapshot("h", at(12, 10, 0));
        write_snapshot(&path, &stored).unwrap();
        assert_eq!(Some(stored.clone()), read_snapshot(&path));
        // Replacing an existing file works and leaves no stray temporary file.
        write_snapshot(&path, &stored).unwrap();
        assert_eq!(1, fs::read_dir(path.parent().unwrap()).unwrap().count());
        // Another version, or garbage, is a miss rather than an error.
        let mut other = stored;
        other.version = SNAPSHOT_VERSION + 1;
        write_snapshot(&path, &other).unwrap();
        assert!(read_snapshot(&path).is_none());
        fs::write(&path, b"{not json").unwrap();
        assert!(read_snapshot(&path).is_none());
    }

    #[test]
    fn the_snapshot_sits_beside_the_index() {
        // Skipped under an environment override, which wins by design.
        if env::var_os("WORKSTATS_NOW_CACHE").is_some() {
            return;
        }
        assert_eq!(
            Path::new("/cache/dir/now.json"),
            snapshot_path(Some(Path::new("/cache/dir/index.sqlite3")))
        );
        assert_eq!(
            Path::new("/cache/dir/now.lock"),
            lock_path(Path::new("/cache/dir/now.json"))
        );
    }

    #[test]
    fn removing_the_snapshot_tolerates_a_missing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("now.json");
        remove_file(&path).unwrap();
        fs::write(&path, b"{}").unwrap();
        remove_file(&path).unwrap();
        assert!(!path.exists());
    }

    // -- building a snapshot from a collected run --------------------------

    fn session(provider: &str, repo: &str, subagent: bool, seen: &[DateTime<Utc>]) -> Session {
        Session {
            provider: provider.to_string(),
            session_id: format!("{provider}-{repo}"),
            cwd: format!("/work/{repo}"),
            repo: repo.to_string(),
            repo_id: format!("local:{repo}"),
            root: format!("/work/{repo}"),
            points: seen
                .iter()
                .map(|timestamp| ActivityPoint {
                    timestamp: *timestamp,
                    model: "claude-opus-5".to_string(),
                })
                .collect(),
            exact_intervals: Vec::new(),
            human_points: Vec::new(),
            token_events: Vec::new(),
            is_subagent: subagent,
            source_file: PathBuf::new(),
            branches: Vec::new(),
            branch_source: BranchSource::None,
            pull_requests: Vec::new(),
        }
    }

    fn tokens(at: DateTime<Utc>, model: &str, output: u64) -> TokenEvent {
        TokenEvent {
            timestamp: at,
            model: model.to_string(),
            usage: TokenUsage {
                output_tokens: output,
                ..TokenUsage::default()
            },
        }
    }

    fn day(date: NaiveDate, human: f64, agent: f64, prompts: usize) -> DayFigures {
        DayFigures {
            date,
            human_seconds: human,
            agent_wall_seconds: agent,
            prompts,
            commits: 1,
            sessions: 2,
        }
    }

    fn inputs<'a>(
        daily: &'a [DayFigures],
        sessions: &'a [Session],
        overrides: &'a RateOverrides,
        goals: Option<&'a Goals>,
        needs_month: bool,
        now: DateTime<Utc>,
    ) -> SnapshotInputs<'a> {
        SnapshotInputs {
            daily,
            sessions,
            overrides,
            goals,
            needs_month,
            now,
            args_hash: "h",
        }
    }

    #[test]
    fn figures_split_today_from_the_week_and_the_month() {
        let now = at(12, 15, 0); // Wednesday; the week began on the 10th.
        let date = |d: u32| NaiveDate::from_ymd_opt(2026, 8, d).unwrap();
        let daily = vec![
            day(date(3), 3600.0, 0.0, 1),    // the month, not the week
            day(date(10), 7200.0, 600.0, 4), // Monday
            day(date(12), 1800.0, 900.0, 5), // today
        ];
        let mut working = session("claude", "workstats", false, &[at(12, 14, 0)]);
        working.token_events = vec![
            tokens(at(3, 10, 0), "claude-opus-5", 1_000_000),
            tokens(at(10, 10, 0), "claude-opus-5", 1_000_000),
            tokens(at(12, 10, 0), "claude-opus-5", 1_000_000),
        ];
        let overrides = RateOverrides::default();
        let each = crate::pricing::list_value_usd(
            "claude-opus-5",
            &working.token_events[0].usage,
            &overrides,
        )
        .expect("the built-in table prices this model");
        assert!(each > 0.0);
        let sessions = [working];

        let result = build_snapshot(&inputs(&daily, &sessions, &overrides, None, true, now));
        let figures = &result.figures;
        assert_eq!(1800.0, figures.human_seconds);
        assert_eq!(900.0, figures.agent_seconds);
        assert_eq!(5, figures.prompts);
        assert_eq!((1, 2), (figures.commits, figures.sessions));
        assert_eq!(
            9000.0, figures.week_human_seconds,
            "Monday and today, not the 3rd"
        );
        assert!((figures.value_usd - each).abs() < 1e-9);
        assert!((figures.value_week_usd - 2.0 * each).abs() < 1e-9);
        assert!((figures.value_month_usd.unwrap() - 3.0 * each).abs() < 1e-9);
        assert_eq!(now, result.computed_at);
        assert_eq!(date(12), result.local_date);

        // The month is not looked up unless something asked for it.
        let skipped = build_snapshot(&inputs(&daily, &sessions, &overrides, None, false, now));
        assert_eq!(None, skipped.figures.value_month_usd);
    }

    #[test]
    fn an_empty_day_is_zero_not_missing() {
        let now = at(12, 15, 0);
        let overrides = RateOverrides::default();
        let result = build_snapshot(&inputs(&[], &[], &overrides, None, false, now));
        assert_eq!(0.0, result.figures.human_seconds);
        assert_eq!(0, result.figures.prompts);
        assert!(result.active.is_none());
        assert_eq!(NowStatus::default(), result.goals);
    }

    #[test]
    fn the_active_session_is_the_newest_foreground_one_with_its_branch() {
        let now = at(12, 15, 0);
        let overrides = RateOverrides::default();
        let mut newest = session("codex", "api", false, &[at(12, 13, 0), at(12, 14, 50)]);
        newest.branches = vec![
            BranchMark {
                from: None,
                branch: "main".to_string(),
            },
            BranchMark {
                from: Some(at(12, 14, 0)),
                branch: "feat/x".to_string(),
            },
        ];
        let older = session("claude", "web", false, &[at(12, 14, 0)]);
        // A subagent seen even later must not count: it is not where you are.
        let helper = session("claude", "helper", true, &[at(12, 14, 59)]);
        let sessions = [older, helper, newest];
        let result = build_snapshot(&inputs(&[], &sessions, &overrides, None, false, now));
        let active = result.active.expect("a foreground session exists");
        assert_eq!("codex", active.provider);
        assert_eq!("api", active.repo);
        assert_eq!(Some("feat/x".to_string()), active.branch);
        assert_eq!(at(12, 14, 50), active.last_seen);
        // Active for ten minutes, not an hour later.
        let snapshot = Snapshot {
            active: Some(active),
            ..snapshot("h", now)
        };
        assert!(active_session(&snapshot, now, Duration::minutes(10)).is_some());
        assert!(
            active_session(
                &snapshot,
                now + Duration::minutes(55),
                Duration::minutes(10)
            )
            .is_none()
        );
    }

    #[test]
    fn goals_reach_the_snapshot_as_a_warning() {
        let now = at(12, 15, 0);
        let goals = Goals::from_config(Some(&serde_json::json!({
            "weekly_hours": 10,
            "list_value_caps": [{"pool": "claude", "period": "month", "usd": 5, "warn_at": 0.5}]
        })))
        .unwrap()
        .unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let daily = vec![day(date, 5.0 * 3600.0, 0.0, 1)];
        let mut working = session("claude", "workstats", false, &[at(12, 14, 0)]);
        working.token_events = vec![tokens(at(5, 10, 0), "claude-opus-5", 1_000_000)];
        let overrides = RateOverrides::default();
        let sessions = [working];
        let result = build_snapshot(&inputs(
            &daily,
            &sessions,
            &overrides,
            Some(&goals),
            true,
            now,
        ));
        assert_eq!(Some(10.0), result.goals.week_target_hours);
        assert_eq!(Some(0.5), result.goals.week_fraction);
        assert_eq!(1, result.goals.warnings.len());
        assert!(result.goals.warnings[0].starts_with("⚠ claude "));
        let line = render_line(
            &Template::parse("{human}{warn}").unwrap(),
            &result,
            false,
            None,
        );
        assert!(line.starts_with("5h00m ⚠ claude "), "{line}");
    }
}
