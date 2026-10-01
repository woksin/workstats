# Branches and pull requests

`workstats` can say which branch, issue and feature work belonged to. The
`branch`, `issue` and `feature` dimensions work with `--group-by` like any
other:

```sh
workstats --month last --group-by issue
workstats --week current --group-by feature,repo
workstats --month 2026-03 --group-by branch
```

- `branch` is the branch name, or `—` when none is known.
- `issue` is the issue key the branch names (`ACME-123`, `#45`), or `—`.
- `feature` is the issue key when there is one, otherwise the branch with its
  prefixes stripped (`feature/login-page` is `login-page`), or `—`. The
  integration branch is `main` (or whatever it is called), not a dash.

Branch names can carry client or ticket names. They are stored in the cache and
printed in reports; review a report before sharing it
([privacy](privacy.md#branch-names-are-read-cached-and-reported)).

## Where the branch comes from

**Sessions.** In order:

1. A branch the provider recorded (`recorded`), when it writes one. A session that moved between branches keeps one entry per change, so
   time is split at the moment of the switch.
2. Otherwise the checkout's HEAD reflog (`reflog`): the branch it was on at the
   moment, read from the `checkout: moving from X to Y` entries. Before the
   first entry on record, it was on the branch that entry left.
3. Otherwise, for a session after the checkout's last switch (or one whose
   checkout never switched), the branch that is checked out now (`head`).
4. Otherwise nothing: the session shows `—`.

Every worktree has its own HEAD reflog, and a session's working directory
identifies exactly one checkout, so a session in a worktree gets that
worktree's branch.

**Commits.** The commit's author date is what places it in time.

1. Find the integration branch. It is the first of `branches.integration`
   (default `main`, `master`, `trunk`, `develop`) that exists as a local
   branch; failing that, the local branch that `refs/remotes/origin/HEAD`
   points at. That ref is read as local metadata: there is no fetch. If there
   is none, every branch commit counts as unique to its branch.
2. A commit reachable from exactly one local branch other than the integration
   branch is on that branch (`unique`).
3. A commit only on the integration branch has usually been merged. The HEAD
   reflogs of the repository's checkouts known to the run (the scanned
   directory, and the checkouts sessions ran in) say which branches were out at
   the commit's time. If exactly one branch other than the integration branch
   was, the commit is on it (`reflog`). That is how a merged and deleted
   branch is recovered.
4. If several were, or none was, the commit is on the integration branch
   (`integration`). Parallel worktrees on different branches make a commit
   ambiguous, and `integration` is the honest answer.
5. A commit only a detached HEAD holds has no branch.

## Configuration

```json
{
  "branches": { "integration": ["main", "trunk"] },
  "issues": {
    "patterns": [
      "(?i)\\bgh-(?P<num>\\d+)",
      "(?P<key>[A-Z][A-Z0-9]+-\\d+)",
      "#(?P<num>\\d+)",
      "^(?P<num>\\d+)-"
    ],
    "projects": ["ACME", "PLAT"],
    "strip_prefixes": ["feature/", "feat/", "fix/", "bugfix/", "hotfix/", "chore/", "refactor/", "users/*/"],
    "fallback": "slug"
  }
}
```

Everything is optional. The `issues` values above are the defaults, except
`projects`, which is empty.

- `branches.integration`: a branch name or a list of names, in order of
  preference.
- `issues.patterns`: regular expressions ([`regex` syntax](https://docs.rs/regex/latest/regex/#syntax))
  applied to the branch name in order; the first that matches wins. Each needs a
  `key` group or a `num` group. A `key` is upper-cased (`acme-1` is `ACME-1`);
  a `num` becomes `#N` (`GH-12` and `gh-012` are `#12`). Naming `patterns`
  replaces the defaults.
- `issues.projects`: adds a case-insensitive `\b(ACME|PLAT)-\d+` matcher after
  the patterns, so `acme-123-login` is `ACME-123`. Without it, Jira-style keys
  are matched case-sensitively so that `release-2` is not an issue.
- `issues.strip_prefixes`: removed from the start of the branch to make its
  slug, repeatedly (`users/ada/feature/login` is `login`). `*` stands for one
  path segment. Naming it replaces the defaults.
- `issues.fallback`: `"slug"` (default), or `"branch"` to use the whole branch
  name for a feature with no issue.

Limits: at most 32 patterns, projects and prefixes, each at most 256 bytes. An
invalid pattern, an unknown key or a wrongly typed value stops the run with an
error naming the key — a misspelled rule set that matched nothing would
report every branch as having no issue.

Only the branch name is read for issue keys. Commit subjects are not.

## Limits worth knowing

- **Stacked branches.** When a commit is reachable from several non-integration
  branches, Git's `--source` names the one it walked first, which is not always
  the one you would pick.
- **Squash merges.** A squash-merged branch leaves no merge commit, and its
  original commits are gone from the integration branch. What is left are the
  squash commit, which is attributed like any commit on the integration
  branch, and the reflog, which can recover the branch if it has not expired.
- **Reflogs expire** (about 90 days by default), so old work on a deleted
  branch reads as the integration branch.
- **Tags checked out by name** look like branches in the reflog. Bare commit
  ids and `HEAD~n` are recognised as a detached HEAD.
- **The integration branch is one branch.** A repository with separate long-lived
  `develop` and `main` branches names only one of them as the integration branch.
- **Process cost.** Per checkout, at most three `git` processes: refs, the
  `--source` pass and the reflog. Nothing is run for a session that recorded
  its branch or whose working directory is no longer a checkout.

## The `branch` and `pr` commands

What did one branch cost, and in whose time? `workstats branch` answers it for
a branch; `workstats pr` answers it in a form you can paste into a pull request.

```sh
workstats branch                       # the branch HEAD is on
workstats branch feat/ACME-1 --base develop
workstats branch --all --month current # one row per local branch
workstats pr                           # Markdown, for a PR description
workstats pr --number 123              # the branch(es) whose sessions linked PR #123
workstats branch --format json
```

Accepted formats are `table` (the default for `branch`), `markdown` (the
default for `pr`), `json` and `html`. `--format csv` is refused. `--no-git` and
`--compare` are refused too: the report is built from the repository's history.

**The window.** It starts at the earliest of the fork point (the author time of
`git merge-base BASE BRANCH`), the branch's first commit, the branch's creation
in the reflog and the first session signal tagged with the branch in this
repository, and ends now. `BASE` is `--base`, else the integration branch
(above). `--since`, `--until`, `--month`, `--week` and `--year` override it; the
start's source is reported as `since_source` in JSON. Without a fork point
(the base branch itself, or unrelated histories) the command asks for `--base`
or a window rather than guessing.

**The figures** are sums over that one collected run, filtered to the branch
and this repository:

- **Human time** is the sum of the human-timeline pieces tagged with the branch.
  Those pieces already partition the human timeline, so time spent on `main` in
  parallel is not counted for the branch, and the rows of `--all` add up to no
  more than the window's total.
- **Agent wall time** is the union of the agent intervals on the branch;
  **parallel agent time** is their plain sum, so the two differ when agents ran
  at once.
- **Tokens, models and list value** come from the sessions' token events, placed
  on the branch at the moment they happened. List value is the tokens priced at
  published rates (`rates as of` is shown), not a bill; models with no price are
  counted in tokens and named in a warning.
- **Commits** are `git rev-list --no-merges BASE..BRANCH`: yours are counted
  with their lines changed, commits an agent authored are counted separately,
  and anything else on the branch (other people's commits) is reported as
  `other` in JSON.

**`--all`** gives one row per local branch that shows any work; the number of
branches left out for showing none is stated. **`pr --number N`** finds the
branches through the sessions whose transcripts mentioned pull request N
([pull-request references](privacy.md#pull-request-references)); if the work
moved across several branches they are listed and also reported combined
(a pooled computation, so parallel agents on two of them are not double-counted).
`pr NAME --number N` names the branch yourself and warns if no session linked
that number to it.

**Markdown is safe to paste.** Branch names, model names and warnings go through
the same escaping as the other Markdown reports, so `fix/#123` does not turn
into a link to issue 123 and `@name` does not mention anyone.

`--describe commits,sessions[=PROVIDERS]` adds a description per branch: the
subjects of your own commits on it and the tool-generated titles of the sessions
that were on it. It is opt-in, read only when asked, never cached, and agent
commits are never asked about; see [opt-in descriptions](privacy.md#opt-in-descriptions).
`branch` and `pr` do not take `--summarize-with`; that is a timesheet option.
