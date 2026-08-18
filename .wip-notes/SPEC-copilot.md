# Design spec: full GitHub Copilot coverage for `workstats`

**Status:** research + design proposal. No repo files were modified; `cargo` was not run.
**Date of investigation:** 2026-08-18
**Machine surveyed:** `darwin 25.5.0`, user `sindrewilting`.

Everything below marked **[verified]** was confirmed by running a command on this
machine during the investigation. Everything marked **[unverified]** is inference or
product behaviour I could not check and should not be treated as fact.

---

## 0. Executive summary

The ask — cover Copilot "both used on system but also used directly on github repos" —
splits into two problems with very different privacy characteristics.

| | Signal | Local-only? | Recommendation |
|---|---|---|---|
| **A** | Copilot CLI (`~/.copilot/session-state/*/events.jsonl`) | yes | **already shipped** |
| **B** | Copilot CLI SQLite store (`~/.copilot/session-store.db`) | yes | adopt as fallback/enrichment |
| **C** | **Copilot Chat in VS Code** (`workspaceStorage/*/chatSessions/*.json`) | yes | **build this — biggest win** |
| **D** | Copilot coding-agent commits in fetched git history | yes | **build this — cheap, honest** |
| **E** | Copilot co-author trailers (review/autofix) | yes | build this with D |
| **F** | Copilot PRs / reviews / Workspace sessions on github.com | **no — needs API** | **do not build into the binary** |

The headline recommendation: **A–E give genuinely broad Copilot coverage with zero
network access.** F is the only part that fundamentally requires the network, and it
should be handled by an out-of-tree `workstats record` producer, not by adding an HTTP
client to `workstats`. Rationale in §5.

---

## 1. What `workstats` reads today

**[verified]** `src/sources.rs:76-78` — the only Copilot source is:

```
("copilot", vec![home.join(".copilot/session-state")])
```

**[verified]** `src/ai.rs:654-661` — `discover_copilot_files` matches exactly one filename:

```rust
pub fn discover_copilot_files(root: &Path) -> Vec<PathBuf> {
    discover_files(root, |path| {
        path.file_name()
            .is_some_and(|value| value.eq_ignore_ascii_case("events.jsonl"))
    })
}
```

**[verified]** `src/sources.rs:128-132` declares Copilot as format `events.jsonl`,
support `best-effort`. `src/sources.rs:10` — `BUILTIN_PROVIDERS = ["claude", "codex",
"copilot", "gemini", "opencode"]`.

**[verified]** `parse_copilot_file` (`src/ai.rs:924-1105`) treats `user.message` records
as human points (`src/ai.rs:1046-1048`), emits one `RawSession` per distinct `cwd` with
`is_subagent: false` (`src/ai.rs:1081`), and emits separate `RawSession`s with
`is_subagent: true` and an `exact_intervals` entry for each subagent task
(`src/ai.rs:1086-1100`). It also drops any record carrying `agentId` or
`parentAgentTaskId` from the foreground timeline entirely (`src/ai.rs:1024-1028`).

Central dispatch is `match provider.as_str()` at **`src/main.rs:450-493`**, whose
`_ => Vec::new()` arm silently drops unregistered providers — a new native adapter that
forgets this arm fails quietly rather than loudly.

So: **only the standalone Copilot CLI is covered, and only its JSONL event log.**

---

## 2. Local Copilot surfaces found on this machine

### 2.1 `~/.copilot/` — the CLI **[verified]**

```
~/.copilot/
  session-state/<uuid>/       19 session dirs
    events.jsonl              <- the ONLY thing workstats reads
    workspace.yaml
    vscode.metadata.json
    checkpoints/ files/ research/
  session-store.db            SQLite, 284 KB   <- NOT read
  session-store.db-wal
  vscode.session.metadata.cache.json
  command-history-state.json  18 KB            <- NOT read
  logs/  ide/  pkg/  config.json  settings.json  permissions-config.json
```

**Event types actually present** across all 19 `events.jsonl` files **[verified]**:

```
855 tool.execution_start     855 tool.execution_complete   842 assistant.message
739 assistant.turn_start     735 assistant.turn_end        372 user.message
352 system.message            66 hook.start                 66 hook.end
 24 permission.requested      24 permission.completed       15 session.shutdown
 14 session.start              9 session.model_change        8 session.mode_changed
  6 session.error              4 abort                       4 session.info
  3 subagent.started           3 subagent.completed          3 skill.invoked
  2 session.compaction_start   2 session.compaction_complete
  2 session.permissions_changed  2 session.context_changed
  1 session.resume             1 session.plan_changed        1 system.notification
```

The current parser handles the important ones. Format looks stable in practice.

### 2.2 `~/.copilot/session-store.db` — a *newer, parallel* CLI store **[verified]**

This is the significant discovery about the CLI. Schema:

```sql
CREATE TABLE sessions (
  id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, host_type TEXT,
  branch TEXT, summary TEXT, created_at TEXT, updated_at TEXT);
CREATE TABLE turns (
  id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, turn_index INTEGER,
  user_message TEXT, assistant_response TEXT, timestamp TEXT,
  UNIQUE(session_id, turn_index));
CREATE TABLE checkpoints (...);  CREATE TABLE session_files (...);
CREATE TABLE forge_trajectory_events (...);  -- empty
CREATE TABLE forge_skill_proposals (...);    -- empty
CREATE TABLE search_index* (FTS5);           -- full-text index over messages
```

Real rows (message bodies deliberately not selected) **[verified]**:

```
id                                    cwd                                   repository                     host_type branch
780e0e2d-...  /Volumes/sourcecode/repos/cratis/AI    Cratis/AI                      github    ai-corpus-from-ada
4108b5e5-...  /Volumes/sourcecode/repos/hive/Ada     Hive-Consulting-Community/Ada  github    main
dc3d80b8-...  /Volumes/sourcecode/repos/cratis/Chronicle  Cratis/Chronicle          github    main
68c65742-...  /Volumes/sourcecode/repos/cratis/Arc   Cratis/Chronicle               github    main   <-- MISMATCH
```

Why this matters:

- **`repository` gives the GitHub `owner/repo` slug with no network call.** That is a
  direct local bridge between a Copilot session and a github.com repository identity.
- `branch` and `host_type` (`github`) come free.
- `turns.timestamp` gives per-turn timing — a second source for human points.

Two cautions:

- **`sessions.repository` can be wrong.** Session `68c65742` has
  `cwd = .../cratis/Arc` but `repository = Cratis/Chronicle` **[verified]**. Trust
  `cwd` over `repository`; treat `repository` as a hint only.
- **`turns.user_message` / `turns.assistant_response` are full prompt and response
  bodies**, and `search_index*` is an FTS5 index over them. `workstats` must never
  `SELECT` those columns. This is exactly the boundary the README promises to hold.
- Coverage is partial: **7 sessions in the DB vs 19 `events.jsonl` dirs** **[verified]** —
  the DB is newer and does not backfill. So it is a *supplement*, not a replacement.

**[unverified]** Whether GitHub intends `session-store.db` to replace `events.jsonl`.
The presence of empty `forge_*` tables suggests active schema evolution. Plan for
`events.jsonl` to remain primary and the DB to be additive.

### 2.3 VS Code Copilot Chat — **the biggest gap** **[verified]**

`~/Library/Application Support/Code/User/workspaceStorage/<hash>/chatSessions/*.json`

120 workspace dirs; the largest `chatSessions` dirs hold 102, 72, 49, 29, 20, 15, 13,
12, 11, 10, 9 session files **[verified]**. Sizes range from 511 B to 6.4 MB.

Top-level shape **[verified]**:

```json
{ "version": 3, "sessionId": "...", "creationDate": 1759815917752,
  "lastMessageDate": 1759827915123, "isImported": false,
  "initialLocation": "panel", "customTitle": "...",
  "requesterUsername": "...", "responderUsername": "...",
  "requests": [ ... ] }
```

Each element of `requests[]` **[verified]**:

```json
{ "requestId": "request_71fa94f9-...", "responseId": "response_ae9d81bb-...",
  "timestamp": 1759815917865,
  "modelId": "copilot/gpt-5-mini",
  "agent": { "extensionId": {"value": "GitHub.copilot-chat"},
             "extensionVersion": "0.31.5",
             "id": "github.copilot.editsAgent",
             "name": "agent", "modes": ["agent"], "locations": ["panel"] },
  "result": { "timings": {"firstProgress": 9485, "totalElapsed": 166924},
              "details": "GPT-5 mini • 1x",
              "metadata": { "renderedUserMessage": [...], "codeBlocks": [] } },
  "message": {"text": "...", "parts": [...]},
  "response": [ ...88 parts... ],
  "variableData": ..., "contentReferences": ..., "codeCitations": ...,
  "followups": ..., "isCanceled": false }
```

Observed `response[]` part kinds **[verified]**: `toolInvocationSerialized`,
`prepareToolInvocation`, `textEditGroup`, `codeblockUri`, `inlineReference`, `undoStop`.

This maps onto the `workstats` model *better than the CLI log does*:

| `workstats` need | VS Code field | Note |
|---|---|---|
| human prompt timestamp | `requests[].timestamp` (epoch ms) | the moment the user hit enter — a true human signal |
| exact agent interval | `timestamp` .. `timestamp + result.timings.totalElapsed` | **exact**, no gap-capping heuristic needed |
| model | `requests[].modelId` (`copilot/gpt-5-mini`, `copilot/claude-3.5-sonnet`) | varies per request within a session **[verified]** |
| session id | `sessionId` | stable UUID |
| cwd / repo | sibling `../workspace.json` → `{"folder": "file:///Volumes/sourcecode/repos/cratis/Fundamentals"}` **[verified]** | direct filesystem path |
| foreground vs agent mode | `agent.id` / `agent.modes` (`["agent"]`, `github.copilot.editsAgent`) | lets ask-mode and agent-mode be told apart |
| version stamp | `agent.extensionVersion` + top-level `version: 3` | good cache-key material |
| tokens | **absent** | `result.details` gives `"GPT-5 mini • 1x"` — a premium-request multiplier, not tokens |

Critical privacy note: `message.text`, `response[]`, and
`result.metadata.renderedUserMessage` **do** contain prompt and response bodies,
including file excerpts. A parser must deserialize only the structural fields above —
exactly the discipline README line 305 already states ("Deserialize only structural
fields; skip prompt and response bodies").

`workspaceStorage/<hash>/chatEditingSessions/<uuid>/` also exists **[verified]** but was
not characterised in depth; it holds edit-undo state, not timing, so it is not needed.

**Stability assessment:** the `version: 3` field and `chatSessions/` layout are VS Code
core (`vscode` repo `chatModel` serialization), not the Copilot extension. It has
changed before (hence `version`) and will change again. **Depend on it as `best-effort`
with a version gate**, the same posture the repo already takes for `copilot` and
`opencode`. Fields I would rely on (`timestamp`, `modelId`, `result.timings.totalElapsed`,
`sessionId`) are the load-bearing ones and least likely to churn; `agent.id` naming is
more volatile.

### 2.4 VS Code global storage **[verified]**

`~/Library/Application Support/Code/User/globalStorage/github.copilot-chat/` contains:

```
session-store.db          <- same schema as ~/.copilot's, plus agent_name/agent_description
                             BUT: sessions=0, turns=0 on this machine (unused)
copilotCli/               copilotCLIShim.js, copilotCLIShim.ps1, copilot,
                          copilotcli.session.metadata.json
vscode-sessions-<uuid>/   diff.index (57 KB–1.3 MB), pathspec.txt
copilot.cli.oldGlobalSessions.json
ask-agent/ plan-agent/ explore-agent/ memory-tool/ debugCommand/
commandEmbeddings.json  toolEmbeddingsCache.bin  logContextRecordings/
```

Finding: VS Code now **embeds the Copilot CLI** via a shim, and carries its own copy of
the CLI's SQLite schema (with extra `agent_name`, `agent_description` columns). On this
machine that DB is empty, so the embedded-CLI path is present but unused. Worth a
detection probe; not worth a parser until it has data.

`vscode-sessions-*/diff.index` is a binary diff cache — not a timing source.

### 2.5 JetBrains — **nothing here** **[verified]**

```
find ~/Library/Application Support/JetBrains ~/Library/Logs/JetBrains \
     ~/Library/Caches/JetBrains -maxdepth 4 -iname "*copilot*"   -> no matches
```

Rider 2025.1 / 2025.2 / 2025.3 are installed; no Copilot plugin artifacts exist.
`~/.config/github-copilot/` is **absent** **[verified]** — that is the classic location
for `hosts.json` / `apps.json`, which are **credential files** and must remain
non-targets under the README's "no credential discovery" promise.

**Recommendation: do not build a JetBrains adapter.** There is nothing to test against
here, and **[unverified]** JetBrains Copilot chat history location/format. Revisit only
if a user supplies a real sample.

### 2.6 Summary of local gaps

| Surface | Present | Parseable | Timing | Model | Repo link | Verdict |
|---|---|---|---|---|---|---|
| CLI `events.jsonl` | yes (19) | yes | yes | yes | `cwd` | shipped |
| CLI `session-store.db` | yes (7) | yes | turn ts | no | `cwd` + slug | supplement |
| VS Code `chatSessions` | yes (~350) | yes | **exact** | yes | `workspace.json` | **build** |
| VS Code global CLI db | yes | yes | — | — | — | empty; detect only |
| JetBrains | **no** | — | — | — | — | skip |
| `~/.config/github-copilot` | **no** | — | — | — | — | never touch (creds) |

---

## 3. Copilot used directly on github.com

### 3.1 What is visible in plain local git after a fetch — **a lot** **[verified]**

Surveyed `/Volumes/sourcecode/repos/cratis/*` and `/Volumes/sourcecode/repos/woksin/*`.

**Commit authors** (counts across all surveyed repos):

```
4556  copilot-swe-agent[bot] <198982749+Copilot@users.noreply.github.com>
  18  Copilot                <198982749+Copilot@users.noreply.github.com>
  13  GitHub Copilot         <copilot@github.com>
```

For contrast, other bots in the same corpus: `github-actions[bot]` 4117,
`dependabot[bot]` 3497, `claude[bot]` 17.

**Co-author trailers** (exact strings, unmasked):

```
225  Co-authored-by: Copilot Autofix powered by AI <223894421+github-code-quality[bot]@users.noreply.github.com>
 39  Co-authored-by: Copilot <copilot@github.com>
 30  Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>
 13  Co-authored-by: copilot-swe-agent[bot] <198982749+Copilot@users.noreply.github.com>
  2  Co-authored-by: Copilot Autofix powered by AI <62310815+github-advanced-security[bot]@users.noreply.github.com>
```

**Important:** the numeric prefix is **not stable** — `198982749+Copilot@` and
`223556219+Copilot@` are both "Copilot" **[verified]**. Match on the local-part suffix
(`+Copilot@users.noreply.github.com`) or on `copilot@github.com`, **never** on the
numeric ID.

**Branches** — the coding agent pushes under a reserved prefix **[verified]**:

```
169  refs/remotes/origin/copilot/*
  7  refs/remotes/origin/copilot-sync/*
  1  refs/heads/copilot/*
```

Real names: `copilot/fix-propagation-workflow-issue`,
`copilot/consolidate-prs-2236-2239-2241-2243-2244`,
`copilot/investigate-publish-pipeline-issues`, …

**Committer identity** for coding-agent-authored commits **[verified]**:

```
462  GitHub <noreply@github.com>                                    (web merge / squash)
167  copilot-swe-agent[bot] <198982749+Copilot@users.noreply.github.com>
 55  Einar <einari@me.com>                                          (human rebased/merged)
```

So author and committer routinely differ, and the human who merged shows up as
committer. A concrete example **[verified]**:

```
AUTHOR:    copilot-swe-agent[bot] <198982749+Copilot@users.noreply.github.com>  2026-05-29 16:50:50 +0000
COMMITTER: Einar <einari@me.com>                                                2026-07-28 16:01:00 +0200
SUBJECT:   feat(captures): use named APIs and add capturer grains
```

Note the two-month author→commit lag. Any adapter must decide which date it means;
`workstats` uses `%aI` (author date) today.

**Volume over time** (coding-agent commits/month, cratis org) **[verified]**:

```
2025-11: 2   2026-02: 38   2026-03: 127   2026-04: 25
2026-05: 222 2026-06: 266  2026-07: 2     2026-08: 2
```

**Signature subject line:** the coding agent's first commit on a branch is literally
`Initial plan` **[verified]**, followed by real subjects. Other observed agent-generated
subjects: `Apply remaining changes`, `Changes before error encountered`. These are
useful as corroboration but must not be the primary matcher — the repo's own design
principle is that commit messages are never read for classification (README lines
84, 351, 381-383). **Keep it that way: match identity, not message.**

### 3.2 What is NOT visible locally **[verified]**

```
git -C .../Chronicle for-each-ref | grep -c refs/pull   ->  0
git config --get-all remote.origin.fetch               ->  +refs/heads/*:refs/remotes/origin/*
```

PR refs are not fetched under the default refspec. Therefore **none** of the following
exist locally: PR numbers, PR open/merge times, review threads, review comment counts,
reviewer identities, Copilot Workspace sessions, or any "Copilot reviewed this" record
that did not result in a co-author trailer.

Also invisible locally: a Copilot code review that produced **suggestions the human
did not accept** leaves no git artifact at all. That work is unobservable without the API.

### 3.3 What requires the GitHub API — verified endpoints

`gh` is installed and authenticated on this machine **[verified]**:

```
gh version 2.71.2
Logged in to github.com account woksin (keyring), protocol ssh
Token scopes: 'admin:public_key', 'gist', 'read:org', 'repo', 'workflow'
```

**PRs opened by the coding agent** — verified working query:

```bash
gh api -X GET search/issues \
  -f q="repo:Cratis/Chronicle is:pr author:app/copilot-swe-agent" -f per_page=3
```

Returned `total_count: 274` **[verified]**, items rendered with `user.login = "Copilot"`:

```json
{"number":3432,"title":"Hash constraint index values to protect PII","user":"Copilot","created_at":"2026-06-29T07:46:30Z","state":"closed"}
{"number":3408,"title":"Fix: Allow coverage statistics job to proceed without cache","user":"Copilot","created_at":"2026-06-17T12:22:40Z","state":"closed"}
```

**PRs reviewed by Copilot code review** — verified working query:

```bash
gh api -X GET search/issues \
  -f q="repo:Cratis/Chronicle is:pr reviewed-by:app/copilot-pull-request-reviewer"
```

Returned `total_count: 11` **[verified]**. So `app/copilot-pull-request-reviewer` is
the real app slug for Copilot code review, distinct from `app/copilot-swe-agent`.

**Review actors on a PR** — `gh api repos/{owner}/{repo}/pulls/{n}/reviews`
**[verified]**, returns `user.login` / `user.type`. On the PRs sampled, bot reviewers
appeared as `github-code-quality[bot]` (type `Bot`) and `github-actions[bot]`.

**Rate limits** **[verified]**: `core` 5000/hr, `search` **30/min**. The search API is
the binding constraint for any per-repo scan.

**Auth required:** a token with `repo` scope for private repos; `public_repo` suffices
for public ones. `gh auth status` shows the token lives in the macOS keyring.

**[unverified]** — I did not check and will not guess at:
- Whether Copilot Workspace sessions are exposed by any REST or GraphQL endpoint at all.
- Whether there is a first-party "Copilot usage/metrics" API that covers an individual
  user (as opposed to org-level admin reporting).
- Whether `app/copilot-swe-agent` is a stable public slug or an implementation detail.
- Whether the coding agent's *working time* (start→finish of an agent run) is exposed
  anywhere. Only `created_at`/`updated_at` on the PR were confirmed, and those are not
  agent runtime.

---

## 4. How each signal maps onto the `workstats` model

### 4.1 The decisive mechanism — `is_subagent`

**[verified]** `src/aggregate.rs:59-61`:

```rust
fn foreground_human_signals(sessions: &[Session]) -> Vec<HumanSignal> {
    let mut signals = Vec::new();
    for session in sessions.iter().filter(|session| !session.is_subagent) {
```

A session with `is_subagent: true` contributes **zero** human signals. It still counts
toward AI wall clock, parallel agent work, and session counts
(`src/aggregate.rs:201`, `270-273`), and still shows up as
`subagent_session_count` in the report (`src/model.rs:257`, `291`).

This is the single lever that keeps the human estimate honest, and it already exists.
**Every github.com-side Copilot signal must land as `is_subagent: true` / `role: subagent`.**
No exceptions.

Secondary safeguard **[verified]** `src/aggregate.rs:177-178`: all human intervals form
one global union, so even a mistake cannot multiply hours — but it could still *extend*
a block, which is why the `is_subagent` gate matters.

Third detail worth knowing when adding signals (`src/timeutil.rs:224-302`): human blocks
are built from **one global ordered timeline**, deduplicated to at most one signal per
distinct timestamp, with priority `*_prompt` (3) > `commit` (2) > `*_session_edge` (1).
Block interiors are sliced at midpoints between adjacent signals, so intervals are
non-overlapping by construction. A new signal kind therefore cannot double-count, but it
*can* bridge two previously separate blocks if it lands in the gap — another reason
github.com-side signals must never enter this timeline.

### 4.2 Git commits — the trap to avoid

**[verified]** `src/git.rs:223-234`:

```rust
.arg("log")
.arg("--regexp-ignore-case")
.arg(format!("--author={author}"))
.arg("--no-merges")
.arg("--date=iso-strict")
.arg("--pretty=format:W%x09%H%x09%aI")
.arg("--numstat");
```

Three consequences:

1. `--author={author}` defaults to the user's own git email (README lines 225-226,
   `src/git.rs:86` `default_git_author`). **Copilot-authored commits are already
   excluded today.** Good.
2. The commit **body is never read** — no `%b`, no trailer parsing anywhere in
   `src/git.rs` **[verified]** by grep. So co-author trailers are currently invisible.
3. Commits are one of the three direct human-evidence signals
   (`src/aggregate.rs:160`, README line 323).

**The trap:** the naive way to "add Copilot coding agent support" is to widen
`--author` to also match `Copilot`. That would push 4556 agent commits into
`filtered_commits`, which feed `human_signals` at `src/aggregate.rs:160-169`, which
create human work blocks with 30-minute review credit each. It would badly inflate the
human estimate — the exact failure the tool exists to prevent.

**The correct design:** run a *second*, separate `git log` pass with an agent-identity
author filter, and route its output to a channel that never reaches
`foreground_human_signals`.

### 4.3 Mapping table

| Signal | Provider | `is_subagent` / `role` | Human time | Counts toward |
|---|---|---|---|---|
| Copilot CLI prompt (`user.message`) | `copilot` | foreground | **yes** (existing) | prompts, AI wall |
| Copilot CLI subagent task | `copilot` | subagent | no | AI wall, parallel |
| VS Code chat `requests[].timestamp` | `copilot-vscode` | **foreground** | **yes** | prompts, AI wall |
| VS Code chat `totalElapsed` interval | `copilot-vscode` | foreground session, exact interval | edges only | AI wall (exact) |
| Coding-agent commit (`copilot-swe-agent[bot]`) | `copilot-agent` | **subagent** | **no** | agent output, git churn |
| `Co-authored-by: Copilot` on *my* commit | — | attribute to existing commit | already human | **no double count** |
| `Co-authored-by: Copilot Autofix` | — | attribute to existing commit | already human | flag only |
| PR opened by coding agent (API) | `copilot-agent` via `record` | **subagent** | no | agent output |
| Copilot code review (API) | `copilot-review` via `record` | **subagent** | no | agent output |

Two subtleties worth being explicit about:

**Co-author trailers are not new work.** If a commit is authored by the user and merely
carries `Co-authored-by: Copilot`, that commit is *already* counted as human evidence.
Emitting an extra agent signal for it would double-count. It should set a *flag* on the
existing commit ("AI-assisted"), not create a new session or interval.

**Agent commits have real churn but no human time.** A `copilot-swe-agent[bot]` commit
should contribute to lines-changed / work-composition / change-shape statistics
*if the user asks for that*, while contributing nothing to `human_estimated_seconds`.
That is a genuinely useful number — "how much landed code did I not type?" — and it is
the honest complement to the human estimate. Default it **off**, because `--author`
scoping is currently the tool's promise about whose work is being measured.

### 4.4 VS Code chat: is a chat prompt "human time"?

Yes, and it is the strongest human signal available for VS Code Copilot. A
`requests[].timestamp` is the instant the developer submitted a prompt — semantically
identical to Claude Code's user message, which the tool already treats as a human point.
Treating it as foreground is consistent, not generous.

`result.timings.totalElapsed` is *agent* time and must only produce an
`ExactInterval` (like `src/ai.rs:1098`), never a human point. Because it is an exact
measured duration, it is strictly better than the gap-capping heuristic
`--gap-cap` applies elsewhere.

---

## 5. The network question — recommendation

**Recommendation: do not add any network capability to the `workstats` binary for
Copilot. Ship A–E (local, zero-network) in-tree, and handle F with a documented,
out-of-tree `workstats record` producer script.**

### Why

1. **The guarantee is unusually strong and specific, and it is a selling point.**
   README lines 407-417 promise "no network calls and no telemetry, unless you
   explicitly run `workstats update` or opt into `--check-updates`" and "no credential
   discovery and no attempt to sign in to providers". `SECURITY.md` line 17 says "The
   only network requests `workstats` ever makes are for checking or [updating]".
   A GitHub API client breaks the second clause too: it must find a token.

2. **Credential handling is the real cost, not HTTP.** Reading a GitHub token means
   touching `gh`'s keyring, `GH_TOKEN`/`GITHUB_TOKEN`, or `~/.config/gh/hosts.yml`.
   The project explicitly lists credential stores as never-discovery-targets
   (README lines 414-415). Even opt-in, this turns a tool with a clean "it cannot
   exfiltrate anything" story into one that must be audited for token handling,
   redaction in `--format json`, and cache poisoning.

3. **The extension point already exists and is a perfect fit.** `workstats record` /
   events-v1 was designed for exactly this: "The open event bridge covers tools such as
   editor agents, internal assistants, SDK calls, and proprietary workflows without
   making `workstats` depend on every vendor's private database schema"
   (README lines 102-104). A GitHub-side Copilot fetcher *is* a proprietary workflow.

4. **The marginal value is low relative to D+E.** Coding-agent commits — the substantive
   output — are already in local git after a fetch. The API adds PR-level bookkeeping
   and unaccepted review suggestions. Nice, not essential.

5. **It would be the only unbounded-runtime, rate-limited, failure-prone path in the
   codebase.** Search API is 30/min **[verified]**; a 40-repo scan is minutes and can
   fail halfway. Everything else in `workstats` is a local file read.

### What to ship instead

A small documented script (shell or Python, in `contrib/`, **not** compiled in) that
shells out to the user's already-authenticated `gh` and pipes into `workstats record`:

```bash
# contrib/copilot-github-sync.sh  (illustrative)
gh api -X GET search/issues \
  -f q="repo:$REPO is:pr author:app/copilot-swe-agent created:>=$SINCE" \
  --jq '.items[] | @json' |
while read -r pr; do
  workstats record \
    --provider copilot-agent \
    --session "pr-$(jq -r .number <<<"$pr")" \
    --model copilot-swe-agent \
    --role subagent \
    --started-at "$(jq -r .created_at <<<"$pr")" \
    --completed-at "$(jq -r .updated_at <<<"$pr")"
done
```

This keeps the binary's guarantee literally intact: the network call is made by `gh`,
under the user's own explicit action, with the user's own credentials, and `workstats`
only ever sees content-free structural records.

**If the owner overrules this** and wants it in-tree anyway, the minimum acceptable shape:
a separate subcommand `workstats sync-github` (never the default report path), refusing
to run without an explicit `--yes-network` flag, reading a token **only** from
`GH_TOKEN`/`GITHUB_TOKEN` env (never from disk, never from a keyring), never persisting
the token to the cache, and a README/SECURITY.md amendment listing it alongside
`update` as a named exception. But I recommend against it.

---

## 6. Implementation sketch

### 6.1 Provider names

| Name | Kind | Source |
|---|---|---|
| `copilot` | existing built-in | `~/.copilot/session-state/*/events.jsonl` (+ optional `session-store.db`) |
| `copilot-vscode` | **new built-in adapter** | VS Code `chatSessions` |
| `copilot-agent` | **new git-derived**, or events-v1 | coding-agent commits / PRs |
| `copilot-review` | events-v1 only | Copilot code review (API-sourced) |

`normalize_provider` (`src/sources.rs:22-31`) should gain aliases:
`copilot-chat` / `vscode-copilot` → `copilot-vscode`; `copilot-swe-agent` → `copilot-agent`.
Note `github-copilot` → `copilot` already exists (`src/sources.rs:27`).

### 6.2 Change A — VS Code Copilot Chat adapter (native, recommended)

**`src/sources.rs`**
- add `"copilot-vscode"` to `BUILTIN_PROVIDERS` (line 10)
- add to `default_history_paths()` (line 70) — note the return type is already
  `Vec<PathBuf>` per provider, so multiple roots are free:
  - macOS `~/Library/Application Support/Code/User/workspaceStorage`
  - Linux `~/.config/Code/User/workspaceStorage`
  - Windows `%APPDATA%/Code/User/workspaceStorage`
  - plus the `Code - Insiders` and `VSCodium` variants
- add a `SourceInfo` row in `source_inventory()` (line 119):
  `("copilot-vscode", "GitHub Copilot Chat (VS Code)", "chatSessions JSON", "best-effort")`
- `source_inventory` detection currently does `path.is_dir()`, which works here.

**`src/ai.rs`**
- `discover_copilot_vscode_files(root)` — glob `*/chatSessions/*.json`. Must be
  scoped to that exact two-level shape; a naive recursive `*.json` walk over
  `workspaceStorage` would pull in unrelated extension state.
- `parse_copilot_vscode_file(path, max_bytes)` — mirror `parse_copilot_file`:
  - resolve cwd from `../../workspace.json` `folder` URI (strip `file://`, percent-decode);
    fall back to `workspace.json`'s multi-root `folders` array **[unverified — I only
    observed the single-`folder` form on this machine]**; if unresolvable, set
    `approximate_cwd: true` as the CLI parser does (`src/ai.rs:1063`)
  - one `RawSession` per file, `provider: "copilot-vscode"`, `is_subagent: false`
  - `human_points` ← each `requests[].timestamp` (epoch ms → `DateTime<Utc>`)
  - `exact_intervals` ← `[timestamp, timestamp + result.timings.totalElapsed]` per request
  - `model` ← `requests[].modelId`, strip the `copilot/` prefix for display
  - `version` ← `format!("copilot-vscode-v{}+{}", version, agent.extensionVersion)` so a
    format change invalidates the cache cleanly (compare `"copilot-v2"` at `src/ai.rs:718`)
  - **serde structs must contain only the structural fields** — no `message`, no
    `response`, no `metadata`. `#[serde(default)]` everywhere; unknown fields ignored.
  - guard on top-level `version`: parse `3`; on anything higher, emit a
    `diagnostics.warn` and skip rather than mis-parse.
- `read_copilot_vscode_sessions_indexed(...)` — clone of `read_copilot_sessions_indexed`
  (`src/ai.rs:697-731`), passing the new discover/parse pair into `load_files`.

**Size guard:** the largest observed session file is **6.4 MB** **[verified]**, and
`chatSessions` dirs hold up to 102 files. These are single JSON documents, not JSONL,
so the existing `MAX_JSONL_LINE_BYTES` streaming discipline does not apply. Either cap
total file size and warn past it, or use a streaming JSON reader. Do not slurp 6 MB ×
350 files into memory unbounded — that would regress the memory numbers the README
advertises (54 MiB warm).

**`src/main.rs`**
- add the dispatch arm in the `match provider.as_str()` at **`src/main.rs:450-493`** —
  without it the provider is silently dropped by the `_ => Vec::new()` arm
- add the import to the `use ai::{…}` list (`src/main.rs:26-29`)
- the existence-pruning retain closure at `src/main.rs:359-367` has an
  `provider == "opencode"` file-vs-dir special case; `copilot-vscode` is a directory, so
  the default `is_dir()` branch is correct and no change is needed there

`--history copilot-vscode=PATH` then works via `parse_history_overrides`
(`src/sources.rs:43`) once the name is in `BUILTIN_PROVIDERS`.

**Complete edit-site checklist for a new native adapter** (13 sites): `BUILTIN_PROVIDERS`
(`sources.rs:10`), `normalize_provider` aliases (`22-31`), `default_history_paths`
(`70-85`), `source_inventory` tuple (`119-140`), detection special-case (`142-146`),
main.rs retain closure (`359-367`), main.rs dispatch match (`450-493`), `read_*_indexed`
+ `discover_*` + `parse_*` in `ai.rs`, the cache provider key and parser-version
fingerprint passed to `load_files` (compare `"copilot"` / `"copilot-v2"` at
`ai.rs:717-718`), the `RawSession.provider` literal in the parser, main.rs imports,
README/CHANGELOG, and the `sources` inventory assertions in `tests/rust_cli.rs:136-139`.

Provider-name validation (`main.rs:658-666` `valid_provider_identifier`,
`ai.rs:1432-1447` `safe_provider`) allows `A-Za-z0-9._/-` after an alphanumeric first
character. `copilot-vscode`, `copilot-agent`, and `copilot-review` all pass.

### 6.3 Change B — `~/.copilot/session-store.db` enrichment (optional)

Open **read-only** (the README already promises this for Codex/OpenCode, line 413 —
use the same `file:...?mode=ro&immutable=1` approach `read_opencode_sessions_indexed`
uses). Query **only**:

```sql
SELECT id, cwd, repository, host_type, branch, created_at, updated_at FROM sessions;
SELECT session_id, turn_index, timestamp FROM turns;
```

Never `user_message`, `assistant_response`, or any `search_index*` table. Use it to
(a) fill `repo` when `events.jsonl` lacks a cwd, and (b) recover sessions whose JSONL
was pruned. Deduplicate against `events.jsonl` by session UUID — the directory name in
`session-state/` is the same UUID as `sessions.id` **[verified]**.

Given `repository` was observed to be **wrong** in 1 of 7 rows **[verified]**, prefer
`cwd` and treat `repository` as a fallback hint only.

### 6.4 Change C — Copilot coding-agent commits (local git, recommended)

New opt-in flag, default off:

```
--agent-commits[=IDENTITY_REGEX]   # default regex matches known Copilot + Claude bots
```

**`src/git.rs`** — add a second collection pass alongside the existing one at line 223,
identical except:

```rust
.arg(format!("--author={agent_author}"))
.arg("--pretty=format:W%x09%H%x09%aI%x09%an%x09%ae")
```

Default identity regex, derived from the verified data and **deliberately not
matching on the unstable numeric ID**:

```
(^|<)(copilot-swe-agent\[bot\]|Copilot)( |<)|\+Copilot@users\.noreply\.github\.com|<copilot@github\.com>
```

Route these to a new `agent_commits` collection. They must **not** be appended to
`human_signals` at `src/aggregate.rs:160`. Surface them as new summary/row fields
(`agent_commit_count`, `agent_additions`, `agent_deletions`) next to the existing
`subagent_session_count`.

Two `src/git.rs` details that matter here **[verified]**:

- `git_regex_literal` (`src/git.rs:103-115`) escapes regex metacharacters, but it is
  applied **only to the default author** from `default_git_author()`
  (`src/git.rs:86-101`). A user-supplied `--author` is passed to git raw. The new
  `--agent-commits=REGEX` should follow the same convention — built-in default is a
  deliberate regex, user overrides are raw — and this must be documented, since the
  default regex above contains intentional alternation.
- The existing pass uses `%aI` (**author** date) and never requests `%cI`/`%cn`/`%ce`.
  Keep that for consistency, and see the lag caveat in §7.

**Branch corroboration** (`refs/remotes/origin/copilot/*`, 169 observed **[verified]**)
is available via `git for-each-ref` but adds little once identity matching works.
Skip it; identity is more reliable than a branch prefix a human could also type.

### 6.5 Change D — co-author trailers (local git)

This one **does** require reading commit bodies, which the repo currently avoids.
The design principle at stake is "never read the commit *message* for classification"
(README lines 84, 381-383) — that is about intent words like "refactor". Reading a
structured `Co-authored-by:` **trailer** for *identity* is a different thing, and is
consistent with the tool's existing identity-based reasoning. But it is a real change
in what gets parsed, so:

- put it behind `--co-authors` (default off)
- use `--pretty=format:...%x09%(trailers:key=Co-authored-by,valueonly,separator=%x02)`
  rather than `%b`, so only the trailer values are ever read into memory — the rest of
  the message is never deserialized
- match the same identity regex as §6.4
- **set a boolean flag on the already-counted commit; create no new session, no interval,
  no human signal.** Report as `ai_assisted_commit_count`.
- keep `Copilot Autofix powered by AI` (`github-code-quality[bot]` /
  `github-advanced-security[bot]`) as a *separate* counter — it is a security-scanning
  autofix product, not interactive Copilot, and conflating them would misrepresent both.

### 6.6 Change E — github.com signals via `workstats record` (no code change)

**[verified]** the events-v1 schema (`schema/workstats-events-v1.schema.json`) already
accommodates this with **no modification**:

- `provider` is an open string, `^[A-Za-z0-9][A-Za-z0-9._/-]*$`, max 64 — `copilot-agent`
  and `copilot-review` both validate.
- `role: "subagent"` is exactly the honesty lever from §4.1.
- `started_at` + `completed_at` (mutually `dependentRequired`) carry the PR interval.
- `cwd` is required — the sync script sets it to the local clone path, which is how the
  event joins the rest of the report.
- The schema **rejects** records containing `content`/`prompt`/`response`/`input`/
  `output`/`api_key`, so a sloppy sync script cannot leak PR bodies into the log.

Example record:

```json
{"timestamp":"2026-06-29T07:46:30Z","provider":"copilot-agent","session_id":"Cratis/Chronicle#3432","cwd":"/Volumes/sourcecode/repos/cratis/Chronicle","model":"copilot-swe-agent","event":"activity","role":"subagent","started_at":"2026-06-29T07:46:30Z","completed_at":"2026-06-29T08:14:02Z"}
```

**Two real limitations of this route** (both **[verified]** against the code):

- **Events v1 cannot carry token counts.** `WorkstatsEvent` (`src/ai.rs:141-166`) has no
  usage field, `parse_event_file` leaves `session.token_events` empty
  (`src/ai.rs:1152`), the schema has no token property, and `record` has no token flag —
  README line 231 states this is deliberate. So an API-sourced Copilot signal can never
  contribute to the AI-tokens column. For Copilot specifically this costs nothing: no
  Copilot surface examined exposes token counts anyway (VS Code gives only a
  `"GPT-5 mini • 1x"` premium-request multiplier). Worth stating so nobody expects it.
- **`record` has no batch or stdin mode** — one process per event. A 274-PR backfill is
  274 forks. Acceptable for a periodic sync; if it becomes a problem, the script can
  emit JSONL itself against `schema/workstats-events-v1.schema.json` and point
  `--events` at the file, which is fully supported (`src/main.rs:381-386`).

Deliverables: `contrib/copilot-github-sync.sh` + a README subsection under
"Add any tool or API" (README line 228) documenting it as **network-using, user-invoked,
out-of-tree**.

### 6.7 Config surface (note, not a required change)

**[verified]** the config file (`src/paths.rs:56-68`, at `$WORKSTATS_CONFIG` →
`$XDG_CONFIG_HOME/workstats/config.json` → `~/.config/workstats/config.json`) currently
holds exactly two keys: `source_roots` and `check_updates`. **No provider history path is
configurable from it** — only `--history PROVIDER=PATH` on the command line, which
*replaces* rather than appends.

That is adequate for this design: `--history copilot-vscode=/path` covers a VSCodium or
portable-install user. But since `default_history_paths()` returns `Vec<PathBuf>` per
provider, this is the natural place to later allow additive roots without a flag. Flag it
as a follow-up, not a blocker. If `--agent-commits` gains a persistent default identity
regex, `Config` is where it belongs.

### 6.8 Docs

- README "Bring your whole AI stack" block (lines 91-98) — add
  `● copilot-vscode  GitHub Copilot Chat (VS Code)  best-effort`.
- README "Privacy boundary" (lines 405-417) — add bullets: VS Code chat sessions are
  read for timestamps/model/duration only; `~/.copilot/session-store.db` is opened
  read-only and message columns are never selected; `~/.config/github-copilot` is
  explicitly a non-target.
- `SECURITY.md` — note the new read locations; **restate that no network exception is
  added**.
- `CHANGELOG.md` entry.
- `tests/rust_cli.rs:136-139` asserts the `sources` inventory ids — must be updated.

### 6.9 Suggested sequencing

1. **§6.4 + §6.5** (git-side, `copilot-agent`): smallest diff, immediately covers the
   "used directly on github repos" ask for the part that matters — landed code.
2. **§6.2** (`copilot-vscode`): biggest coverage win, largest diff.
3. **§6.3** (SQLite enrichment): optional polish.
4. **§6.6** (`contrib/` sync script): documentation + script only.

---

## 7. Risks and unknowns

**Format stability**
- VS Code `chatSessions` is VS Code core serialization with an explicit `version: 3`
  field — it *will* change. Mitigation: version gate + `best-effort` labelling + a
  version-stamped cache key. **[unverified]** how often it has historically bumped.
- `~/.copilot/session-store.db` has `schema_version` = 1 and two empty `forge_*` tables,
  suggesting in-flight schema work. **[unverified]** whether it will supersede
  `events.jsonl`. If it does, the existing `copilot` adapter silently goes quiet —
  worth a diagnostic that warns when `session-store.db` has sessions newer than the
  newest `events.jsonl`.

**Identity matching**
- The numeric prefix in `NNN+Copilot@users.noreply.github.com` is **not stable**
  (198982749 and 223556219 both observed **[verified]**). Regex must not depend on it.
- `GitHub Copilot <copilot@github.com>` (13 commits **[verified]**) is a distinct
  identity from `copilot-swe-agent[bot]`. **[unverified]** which product emits it —
  possibly an older coding-agent version or a GitHub.dev/web-editor commit.
- A human could trivially set `user.name = "Copilot"` and be miscounted. Low risk,
  but the regex should be documented and overridable.
- `copilot-sync/*` branches (7 **[verified]**) — **[unverified]** what produces these.
  Do not match on them without knowing.

**Double counting**
- A coding-agent PR that is squash-merged appears *both* as a `copilot-swe-agent[bot]`
  authored commit **and** (via §6.6) as a recorded PR event. Both are `subagent`, so
  neither inflates human time, but AI wall clock could double-count. Mitigation:
  document that §6.4 and §6.6 are alternatives, not complements, for the same repo.
- Committer-vs-author lag can be months (`2026-05-29` author → `2026-07-28` commit
  **[verified]**). Since `workstats` uses `%aI`, agent commits land in the report on the
  date the agent wrote them, not the date they merged. This is defensible but surprising
  and should be documented.

**Coverage honesty**
- VS Code inline completions / ghost-text (the original Copilot product) leave **no
  timestamped local artifact I could find**. Any claim of "full Copilot coverage" would
  overstate; the honest claim is "Copilot CLI, Copilot Chat in VS Code, and Copilot
  coding-agent commits".
- Copilot review suggestions that a human rejects are invisible without the API,
  permanently.

**Performance**
- 350+ chat session files, up to 6.4 MB each **[verified]**, is a materially different
  IO profile from JSONL streaming. Without a size cap or streaming parser this can
  regress the advertised 54 MiB / 1.05 s warm numbers.
- 120 `workspaceStorage` dirs must be walked to find the ~26 with sessions — cheap, but
  the glob must be tight.

**Not investigated**
- Windows and Linux VS Code paths — asserted from convention, **[unverified]** on this
  machine (macOS only).
- `Code - Insiders` / VSCodium presence — **[unverified]**, neither installed here.
- `chatEditingSessions/` contents beyond directory listing.
- JetBrains Copilot entirely — **[verified absent]** here, so no format to design against.
- Whether Copilot Workspace produces *any* local or API-visible artifact — **unknown**.

---

## 8. Bottom line

Ship the local work: a `copilot-vscode` adapter (§6.2) plus agent-commit and co-author
detection in git (§6.4, §6.5). That covers "used on system" properly for the first time
and covers the substantive part of "used directly on github repos" — the code that
actually landed — **without touching the network guarantee.**

Do not put a GitHub API client in the binary. Route PR-level and review-level Copilot
activity through `workstats record` with `role: subagent`, driven by a documented
`contrib/` script that uses the user's own `gh`. The events-v1 schema already supports
this unchanged, and the `is_subagent` gate at `src/aggregate.rs:61` already guarantees
none of it can inflate the human-work estimate.
