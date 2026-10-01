# Privacy boundary

What `workstats` reads, what it never reads, and the one place file contents are shown.

`workstats` is local-only by default:

- no network calls and no telemetry, unless you explicitly run `workstats
  update` or opt into `--check-updates` (see [Updating](install.md#updating));
- no credential discovery and no attempt to sign in to providers;
- no prompt or response bodies in reports or the cache;
- Git is read for commit metadata only — the commit id, the author date, and
  `--numstat`'s per-path line counts — from every local branch of each
  repository, never a remote. The second pass
  [`--agent-commits`](how-it-works.md#agent-authored-commits) runs is the same read with a
  different `--author`, over the history already on disk, and it makes no network
  call. `--co-authors` widens that read by exactly the *values* of
  `Co-authored-by:` trailers, asked for by name; no other part of a commit
  message is ever requested, and commit messages are never read to classify
  anything;
- no file contents in reports or the cache either — the explorer's diff viewer
  is the single place a tracked file is ever read, and what it reads is
  display-only ([details below](#the-diff-viewer-is-the-one-place-file-contents-are-read));
- a VS Code chat session is read for timestamps, model ids, and how long each
  turn took; the parser names no message field, so prompt and response bodies
  are never deserialized;
- Copilot's `~/.copilot/session-store.db` is read for `sessions(id, cwd,
  repository, branch, host_type)` and nothing else — that database also holds a
  `turns` table of full prompt and response bodies and a `search_index` FTS5
  index over them, and `workstats` queries neither;
- Codex, Copilot, and OpenCode SQLite databases are opened read-only;
- known credential locations such as `auth.json`, `secrets.json`, `.env`,
  `~/.config/github-copilot/`, and key stores are never discovery targets;
- malformed and oversized transcript records degrade safely;
- CSV cells are neutralized against spreadsheet formula injection.

The cache contains the structural fields needed for reports: timestamps,
working directories, session identifiers, model names, roles, derived
intervals, and token usage counts. JSON/CSV output can contain repository
names and paths—review a report before sharing it. `--explain-human-time` is an
explicitly more detailed view: it includes exact UTC signal/block timestamps,
signal kinds, providers, and repository labels, but still excludes prompt and
response text, session identifiers, working-directory/root paths, model names,
and commit hashes.

### The diff viewer is the one place file contents are read

Every number `workstats` reports is derived from paths, timestamps, and line
counts. The explorer's deepest level is the one exception, and it is
deliberately narrow: when you press `Enter` on a changed file in `workstats
ui`, it runs Git in that repository and shows you the patch.

That patch is **display-only**, and specifically:

- it is **never written to the cache**;
- it is **never written into a report** — not the dashboard, not `--format
  json`, not `--format csv`;
- it is **never stored in a saved view**; a saved view is a drill-down path and
  a sort, it cannot even name the diff level, so restoring one never reopens a
  file;
- it is **never sent anywhere** — reading a diff makes no network call, exactly
  like every other part of a normal run;
- it exists **only in memory while it is on screen**, and is dropped the moment
  you navigate away.

Nothing else changes. Reports, the cache, and the event format still contain no
prompt bodies, no response bodies, and no file contents; `workstats` without
`ui` never opens a tracked file at all. The viewer reads only what you are
already entitled to read — it shells out to your own `git`, in your own
checkout, and only for a commit id it has validated as a plain hexadecimal
object name, with the file path passed after `--` so it cannot be read as a
flag.

Two practical limits: a diff is truncated at roughly 2 MiB, 20,000 lines, or
2,000 characters per line and says so in the footer; and control and
direction-override characters in the patch are replaced before it is drawn, so
a file cannot repaint or reorder your terminal. Binary files come back as
Git's own `Binary files … differ` line — no bytes are ever emitted.

## Branch names, pull requests, descriptions, bundles and snapshots

These sections are filled in by the changes that add each read. Each states
exactly what is read, what is stored, and what is never read.

### Branch names are read, cached and reported

_Coming in this release._

### Pull-request references

_Coming in this release._

### Opt-in descriptions

_Coming in this release._

### `--summarize-with` hands a digest to a command you choose

_Coming in this release._

### Files beside the config: the timesheet ledger and the machine id

_Coming in this release._

### Bundles

_Coming in this release._

### The `now` snapshot

_Coming in this release._

See [SECURITY.md](../SECURITY.md) for private vulnerability reporting.
