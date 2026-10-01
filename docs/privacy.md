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

### Engagement configuration and timesheet outputs

`engagements` in the config names clients, rates, currencies and the paths,
remotes and branch patterns that identify them. It is read from the config file
you wrote, never discovered, and it is not stored in the cache. The consequences
to know about:

- `workstats timesheet` prints those names and rates, and the amounts computed
  from them, in its table, JSON, CSV, Markdown and HTML output. Treat a
  timesheet as you would an invoice: anything you save, mail or paste carries
  the client names, rates and hours.
- The same keys appear as group names under `--group-by engagement`.
- A timesheet adds no new reads. It is computed from the timeline the report
  already built, so it never reads prompts, responses, file contents or commit
  messages, and it makes no network call. Its evidence columns are counts
  (prompts, commits, sessions), plus repository, branch and issue names.
- The vendor CSV presets (`--export`) add the `timesheet.person` email and name
  from the config, if you set them, and nothing else about you.
- Descriptions are not part of this: they are opt-in and covered under
  [Opt-in descriptions](#opt-in-descriptions).

### Branch names are read, cached and reported

**From AI tools.** Some providers record which Git branch a session was on. workstats now reads
that one field, and only that field, from each:

| Provider | What is read |
|---|---|
| Claude Code | `gitBranch` on `user` and `assistant` records |
| Codex | `payload.git.branch` of the rollout's `session_meta` record, and the `git_branch` column of the `threads` table |
| Copilot CLI | `data.context.branch` of `session.start` and `session.context_changed`, and the `branch` column of the session store (already read for the working directory) |
| Events | the optional `branch` field of a record, which `workstats record --branch NAME` writes |

Pi, OpenCode, Gemini and VS Code Copilot record no branch and are unchanged.

Nothing else beside those fields is read, in particular:

- **Codex** `threads.title`, `threads.preview` and `threads.first_user_message`
  are the first prompt verbatim, so they stay out of the closed column list in
  the Codex reader. The commit hash and repository URL next to
  `payload.git.branch` are not read either.
- **Claude** `last-prompt` and `queue-operation` records carry prompt text and
  `ai-title` / `agent-name` carry generated titles. The parser declares no
  field for any of them, so their content is skipped by the deserializer
  without being held in memory.

A branch name is stored only when it changes (at most 256 changes per session),
at most 256 bytes long, and without control characters; anything else is
dropped and counted in the run's notes. The names are stored in the transcript
cache and appear in reports under the `branch`, `issue` and `feature`
groupings.

**Branch names can carry client names, ticket numbers or personal names**
(`acme/ACME-123-login`, `users/ada/spike`). They are exactly as sensitive as
your branch list. A report grouped by branch, or its JSON, shares them; a plain
report does not show them.

Deleting the cache (`--rebuild-cache`) removes the stored names; the history
files they came from are never altered.

**From Git.** To say which branch a commit or a session belongs to,
`workstats` reads local ref names and nothing else, with no network call and
no fetch:

- the names of the local branches, which branch is checked out, and the *name*
  `refs/remotes/origin/HEAD` points at (`git for-each-ref`);
- the local branch each recent commit not on the integration branch is
  reachable from (`git log --source`, asking for the hash and the ref name
  only, over the window widened as the commit listing is, and for every author:
  other people's hashes are matched against yours and nothing else is kept);
- the checkout's HEAD reflog, filtered by Git itself with
  `--grep-reflog='^checkout: moving from '` so that only branch switches come
  back. The other reflog entries hold your commit messages (`commit: <subject>`)
  and are never delivered to `workstats`; the switch entries hold two ref
  names.

At most three `git` processes run per checkout. Commit messages, bodies and
file contents are not read for this.

**What is stored and shown.** Branch names are cached with the sessions they
belong to, and reported under the `branch`, `issue` and `feature` groupings.
Branch names can carry client or ticket names, so review a report before
sharing it. Issue keys are cut from the branch name only. See
[Branches and pull requests](branches.md).

### Pull-request references

Two providers link a session to a pull request, and workstats keeps only the
pull request's number and repository:

- **Claude Code** `pr-link` records: `prNumber` and `prRepository`. The
  record's `prUrl` is not declared by the parser, so it is never read or stored.
- **Copilot CLI** `session_refs` rows with `ref_type = 'pr'` in the session
  store. The query selects the session id and the reference value and filters on
  the type, so the `commit` rows are not delivered. A value may be a bare number,
  `owner/repo#number` or a pull-request URL; only the number and the
  `owner/repo` pair are kept, and the URL, host and query string are discarded.
  Without a repository in the value, the session's own `repository` column is
  used, which Copilot itself sometimes gets wrong.

A session keeps at most 64 references. They are cached with the session and are
used to find the branch a pull request was worked on; they are not shown in an
ordinary report.

### `insights` and `digest` read nothing new

`workstats insights` and `workstats digest` are computed from the data a report
is built from: no additional file, database column or Git command is read. What
they add is presentation, and some of it is worth knowing before you share it:

- `digest` lists the **top repositories and features** by name. A feature is the
  issue key or branch name (see above), so a digest pasted into a PR or a status
  update carries those names. Warnings in the Markdown and HTML have your home
  directory redacted, as in the report.
- `insights --section heatmap` shows when you work (weekday and hour, late
  nights, weekends). That is a pattern of your life, not just your projects.
- `models` shows which providers and models you used and their list value.
  List value is the tokens priced at published rates, not a bill.

Nothing from a prompt or a response is involved, and nothing is sent anywhere.

### Opt-in descriptions

_Coming in this release._

### `--summarize-with` hands a digest to a command you choose

_Coming in this release._

### Files beside the config: the timesheet ledger and the machine id

`machine.json`, written by `workstats export` next to the config file, holds a
random 128-bit id and a label for this machine (`--label`, or `HOSTNAME` /
`COMPUTERNAME`). Nothing is read from the system to make the id; it identifies
nothing about the machine and is sent nowhere. It appears only inside bundles
you export, where it keeps one machine's `local:` repository keys apart from
another's. It is never regenerated if the file is unreadable.

### Bundles

`workstats export` writes a `workstats-bundle` so another machine can merge your
history (see [Merging machines](merge.md)). It holds the same structural fields
the transcript cache holds — activity timestamps, model names, token counts,
session ids, branch names and pull-request numbers — plus:

- repository identity keys: the normalised fetch remote (`remote:github.com/acme/api`),
  a project alias key, or, for a repository with no shareable remote, an opaque
  `local:<machine id>:<label>` key. A remote that names a directory on disk is
  treated as having no remote, so it cannot carry a path out;
- for each session, the working directory **relative to the repository root**
  (`src/api`), never the absolute path;
- per commit: SHA, author time, line counts, per-category line tallies, the
  agent-authored / assisted flags and the branch name;
- the machine's id and label, the Git author patterns used, the export window
  and the idle/credit/gap settings.

**Never in a bundle:** prompts or responses, commit subjects or bodies, session
titles, absolute paths. Changed file paths appear only when you pass
`--include-paths`, and are then repo-relative. Bundles are plain JSON and **are
not encrypted**: anyone who has the file can read the dates and hours it
implies, the repository names and branch names (which can carry client or
ticket names), and the commit SHAs. Share them the way you would share the
report they produce.

Import reads a bundle with strict bounds (at most 1 GiB, a known format and
version, every repository key and provider checked, labels and subdirectories
reduced to plain relative names) and never executes or follows anything in it.
Imported work gets a synthetic working directory (`api@laptop/src`) that is not
a path on your machine, so nothing scans it as one.

### The `now` snapshot

_Coming in this release._

See [SECURITY.md](../SECURITY.md) for private vulnerability reporting.
