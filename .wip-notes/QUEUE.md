# workstats — outstanding work queue

Kept by the orchestrator. Items move to DONE only after verification, not after
an agent claims completion.

## DONE (verified)

- v0.8.0 — work composition, change shapes, committed output
- v1.0.0 — gitstats alias and its whole compatibility surface removed
- v1.0.1 — root-level generated directories ignored again (`node_modules/`, `dist/`, `bin/`)
- v1.0.2 — .NET `*.Specs` projects and BDD `for_`/`when_`/`given` folders counted as tests
- README: broken Homebrew instructions removed (docs-only PR #10, no release cut)
- Homebrew tap set up end to end and VERIFIED:
  repo `woksin/homebrew-workstats` public · read-write deploy key · `HOMEBREW_TAP_DEPLOY_KEY`
  secret · key reaches remote over SSH · formula published for v1.0.2 with all four
  SHA256s matching the release · `brew tap woksin/workstats` succeeds · `brew info`
  resolves `workstats 1.0.2`. Backfilled onto the existing release, no new version cut.
- Local install updated to v1.0.2 and the test-detection fix confirmed present in it.

## IN FLIGHT

- **Main workflow** (`wf_2c6f10ad-961`), branch `feature/explorer-and-configurable-categories`:
  Foundation (category registry + git/output fixes) → Fixes (AI adapters + timeutil)
  → TUI (core, views, search+diff) → Integrate → Docs → adversarial Review.
- **Release-workflow hardening** — COMPLETE, awaiting review + PR split. Touches only
  `.github/workflows/release.yml`. Independent of the feature branch, so it should be
  extracted into its own PR rather than riding along.

## QUEUED — blocked on the main workflow releasing file ownership

### 1. Month shorthand  (owner: `src/main.rs`, `src/timeutil.rs`)
Requested: a shorthand for the `--since X --until X` pair for one calendar month.
Design:
  * `--month YYYY-MM` sets both bounds to that month. Add `--year YYYY` for symmetry.
  * `conflicts_with` `--since` and `--until` so the combination errors instead of one
    silently winning — the repo already has this bug shape with
    `--by-repo`/`--matrix`/`--by-dir`, which the integration agent is fixing.
  * Consider bare `--month` = current month, and `last`/`previous` for the prior one.
    Relative values are genuinely useful for a recurring monthly report.
  * NAMING CARE: `--period month` and `--group-by month` already exist and are
    GROUPING controls, whereas this is a FILTER. Document the distinction explicitly
    or the two will be confused; consider `--in` as an alternative name.
  * Test the boundary: `--month 2026-01` must include 2026-01-31T23:59 local and
    exclude 2026-02-01T00:00 local. The coverage audit flagged that `--since`/`--until`
    semantics span three layers (`parse_bound`, `git log`'s inclusive bounds, and the
    aggregator's `>= since && < until`) with no end-to-end test.

### 2. GitHub Copilot coverage  (owner: `src/ai.rs`, `src/git.rs`, new `contrib/`)
Full design in `scratchpad/SPEC-copilot.md`. Ship the local surfaces in the binary;
keep the network out of it.
  * VS Code Copilot Chat: `~/Library/Application Support/Code/User/workspaceStorage/
    <hash>/chatSessions/*.json` — per-request timestamps, `modelId`, and
    `result.timings.totalElapsed` (an EXACT agent duration, better than `--gap-cap`).
    Sibling `../workspace.json` maps to the repo folder. JSON not JSONL, so the
    streaming discipline does not apply — needs a size cap (files reach 6.4 MB).
  * `~/.copilot/session-store.db`: use `sessions.repository`/`branch` only.
    ITS `turns` TABLE HOLDS FULL PROMPT AND RESPONSE BODIES PLUS AN FTS5 INDEX —
    NEVER SELECT THEM. Supplement, not replacement (7 sessions vs 19 event dirs), and
    its `repository` was wrong in 1 of 7 rows, so prefer cwd when they disagree.
  * GitHub-side: match bot identity on the email SUFFIX, never the numeric prefix —
    both `198982749+Copilot@` and `223556219+Copilot@` occur in real history.
    THE TRAP: do NOT widen `git.rs`'s `--author` filter. 4,556 agent commits would
    enter `human_signals` (`aggregate.rs:160`), each spawning a work block with 30 min
    review credit — the exact inflation this tool exists to prevent. Use a SECOND,
    separate `git log` pass whose output never reaches `foreground_human_signals`.
    Co-author trailers must flag an already-counted commit, not create a new signal,
    or they double-count.
  * PR/review data needs the GitHub API. Do NOT add an HTTP client — that breaks
    "no credential discovery" as well as "no network calls". Route it through a
    `contrib/` script using the user's own `gh` and `workstats record`.
  * Do NOT build a JetBrains adapter: nothing exists on this machine despite three
    Rider installs, and `~/.config/github-copilot/` holds credentials so it must
    remain a non-discovery target.
  * Every GitHub-side signal lands as `role: subagent` so it contributes to AI
    wall-clock and agent output but ZERO human time.

### 3. Restore the README Homebrew section
The tap now works, so the instructions should come back. Prefer the one-command
`brew install woksin/workstats/workstats`, which auto-taps AND auto-trusts, grants
formula-level rather than whole-tap trust, and works on Homebrew 5.
CORRECTION for the old text: the trust gate landed in Homebrew **5.1.15**, not 6.0.
ORDERING IS NOW SAFE: the secret exists, so the hardened guard's "advertised but not
configured" failure branch will not fire.

### 4. Audit items not covered by the main workflow
From `scratchpad/AUDIT.md`, the UX/consistency tail (items S, T, and most of V):
  * `--raw` renders a flat GLOBAL model list indented as if nested under the last
    provider, and its `.take(12)` truncation is silent.
  * `<synthetic>` leaks into user-facing output alongside `unknown` and `—` — three
    spellings of "no model".
  * `--top` bounds the table but not the sparkline built from all rows.
  * Summary block label widths inconsistent (values land in columns 23/22/25); banner
    rule is 94 wide against a 106-wide table rule.
  * CSV lacks `change_shapes`, summary, methodology, diagnostics that JSON carries.
  * CSV `first_seen`/`last_seen` are UTC while `day`/`month` group keys are local, so
    re-deriving the month from `first_seen` contradicts the `month` column.
  * JSON duplicate aliases: `agent_wall_seconds` == `deduplicated_active_seconds`;
    `parallel_agent_seconds` == `attributed_active_seconds` == row `active_seconds`.
  * Zero tests on `print_table`, the DEFAULT output (~265 lines, ~12 branches).

## ORCHESTRATOR NOTES

- Nothing is built or tested until the main workflow finishes — the repo owner's
  explicit instruction. The first compile will surface cross-agent type mismatches;
  expect to work through them.
- Split the final result into logical PRs rather than one giant one. At minimum the
  release-workflow hardening is independent and should ship separately.
- Verify every release after merging. I merged PR #9 and did not check that it cut
  v1.0.2; a subagent caught it. Check the release, its assets, and the shipped binary.
