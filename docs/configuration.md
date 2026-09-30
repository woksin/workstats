# Configuration

The config file, its keys, defaults, rates, categories and aliases, plus the inputs `workstats` reads and the index it keeps.

## Inputs and index

| Source | Default location |
| --- | --- |
| Codex sessions | `~/.codex/sessions` |
| Codex metadata | `~/.codex/state_5.sqlite` |
| Claude Code projects | `~/.claude/projects` |
| Gemini CLI sessions | `~/.gemini/tmp/*/chats` |
| GitHub Copilot CLI sessions | `~/.copilot/session-state/*/events.jsonl` |
| GitHub Copilot CLI metadata | `~/.copilot/session-store.db` |
| GitHub Copilot Chat sessions | `~/Library/Application Support/Code/User/workspaceStorage` (macOS) |
| OpenCode sessions | `~/.local/share/opencode/opencode.db` |
| Pi sessions | `~/.pi/agent/sessions/--<encoded cwd>--/*.jsonl` |
| Workstats Events | platform data directory (shown by `workstats sources`) |
| Git repositories | current directory, `--dir PATH`, or `WORKSTATS_DIR` |

VS Code keeps that chat directory under `%APPDATA%\Code\User\workspaceStorage`
on Windows and `~/.config/Code/User/workspaceStorage` on Linux. `Code -
Insiders` and `VSCodium` are read alongside `Code` when they exist; any other
install is reachable with `--history copilot-vscode=PATH`.

Pi resolves its own session directory from `PI_CODING_AGENT_SESSION_DIR`, then
`PI_CODING_AGENT_DIR`, then `~/.pi/agent`, and `workstats` reads the same two
variables — so a relocated or containerised install is found rather than
silently reported as no activity.

All inputs are optional (`--no-git`, `--no-ai`, `--provider`, and
`--exclude-provider`). Missing default histories are silently skipped; a
mistyped `--history` or `--events` path is reported as a `Warning:` line under
the report (the first five, with a count of the rest; `--format json` carries
them all) rather than producing a clean-looking report with data quietly
missing. The structural index defaults to:

| Platform | Cache | Config | Event log |
| --- | --- | --- | --- |
| macOS | `~/.cache/workstats/index.sqlite3` | `~/.config/workstats/config.json` | `~/Library/Application Support/workstats/events.jsonl` |
| Linux | `$XDG_CACHE_HOME/workstats/index.sqlite3` or `~/.cache/workstats/index.sqlite3` | `$XDG_CONFIG_HOME/workstats/config.json` or `~/.config/workstats/config.json` | `$XDG_DATA_HOME/workstats/events.jsonl` or `~/.local/share/workstats/events.jsonl` |
| Windows | `%LOCALAPPDATA%\workstats\cache\index.sqlite3` | `%APPDATA%\workstats\config.json` | `%LOCALAPPDATA%\workstats\events.jsonl` |

```bash
workstats --rebuild-cache       # rebuild every indexed entry
workstats --no-cache            # one uncached invocation
workstats --cache /safe/path.db # choose another index
workstats --config ./team.json  # read source roots and categories from elsewhere
```

The config file holds `source_roots`, `categories`, `category_mode`,
`project_aliases`, `authors` (a string for one identity, or a list), `model_rates` (list-rate overrides for
[`allocate`](allocate.md#rates-and-when-they-go-stale)), `check_updates`, and `defaults`
(below). `workstats ui`'s saved views are kept beside it as
`views.json` — configuration, never cache — so `--rebuild-cache` and
`--no-cache` leave them alone.

`defaults` holds the flags you would otherwise retype on every run. Each key is
the flag's long name in snake_case, and each only fills in a flag that was not
given: flag, then environment variable (`WORKSTATS_DIR` for `dir`), then config
default, then the built-in default. A flag typed with its built-in value, such
as `--depth 4`, still beats the config.

```json
{
  "defaults": {
    "dir": "~/code",
    "depth": 3,
    "format": "table",
    "providers": ["claude", "codex"],
    "group_by": "repo,month",
    "gap_cap": "10m",
    "human_idle": "90m",
    "review_credit": "20m"
  }
}
```

| Key | Flag | Value |
| --- | --- | --- |
| `dir` | `--dir` | Directory to scan; `~` expands to your home directory |
| `depth` | `--depth` | Whole number |
| `format` | `--format` | Any `--format` value |
| `providers` | `--provider` | List of provider names, or one comma-separated string |
| `group_by` | `--group-by` | Comma-separated dimensions; ignored when `--by-repo`, `--matrix`, or `--by-dir` is given, and its `day`, `week` or `month` gives way to an explicit `--period` (other dimensions stay) |
| `gap_cap`, `human_idle`, `review_credit` | `--gap-cap`, `--human-idle`, `--review-credit` | Durations such as `30s`, `5m`, `1h` |

An unknown key or an invalid value stops the run with a message naming it, for
example `invalid defaults.gap_cap "soon"` or a misspelt key such as `depht`,
rather than being ignored. `workstats ui` ignores `format`, and `allocate` ignores `group_by`,
because each sets that itself. `--format json` lists the values taken from the
config under `inputs.config_defaults` (including `dir` when the config's
directory was the one scanned), and `allocate --format json` carries the same map as
`config_defaults`. The table, Markdown, HTML and `allocate` outputs end their notes with
a one-line `Config defaults: …` when any applied; `allocate --format csv` has no
place for it and omits it.
A refusal caused by a configured format, such as `--compare` with
`defaults.format "csv"`, names the config and suggests `--format table` or
`--format json`.

`model_rates` is checked the same way as `defaults`: a misspelt field (`inptu`),
a wrong type, or a bad value stops the run with an error naming the entry, for
example `invalid "model_rates" configuration: invalid model_rates.acme-coder`,
and the rest of the file is never discarded for it.

`WORKSTATS_CACHE`, `WORKSTATS_CONFIG`, `WORKSTATS_EVENTS`, `WORKSTATS_VIEWS`,
`WORKSTATS_DIR`, and `WORKSTATS_GIT` provide explicit overrides. When
[update checks](install.md#updating) are opted into, the last known result is cached next
to the index as `update-check.json` (`WORKSTATS_UPDATE_CACHE` overrides its
path); it contains only a version string and a timestamp.

Copilot and OpenCode are marked best-effort because their vendors may evolve the
internal event/database/document schema. The readers check tables and fields
before querying, stay read-only, and degrade to diagnostics instead of guessing:
a chat session in a format newer than the parser knows is reported and skipped
rather than guessed at.

A Copilot CLI session whose event log never recorded a working directory takes
one from `session-store.db`, so it is attributed to the repository it actually
ran in instead of to the transcript directory. A directory the event log did
record always wins, and a `repository` in the store that disagrees with the
directory is reported as a diagnostic naming both rather than silently
preferred.

Token usage (input, output, cache-read, cache-creation) is read from Claude
Code and Codex transcripts directly and from Copilot's end-of-session summary.
Gemini CLI and OpenCode token counts are best-effort and may read as zero if
the locally installed version doesn't expose per-turn usage fields; this never
affects the time-based metrics.

## Make the areas your own

The six built-ins are defaults, not a closed set. The `categories` block in the
[config file](#inputs-and-index) adds rules to a built-in area, and any name the
built-ins do not know creates a **new** area that appears everywhere the others
do — the dashboard, JSON `composition`, CSV columns, change shapes, and the
explorer:

```json
{
  "categories": {
    "test":     {"directory_prefixes": ["it_"]},
    "ai":       {"directories": [".ai", ".claude"], "names": ["CLAUDE.md", "AGENTS.md"]},
    "planning": {"directories": ["planning", "rfcs"], "name_suffixes": ["-plan.md"]},
    "corpus":   {"directories": ["corpus"], "extensions": ["jsonl"]}
  },
  "category_mode": "extend"
}
```

`category_mode` decides what happens to a name the built-ins already know:

- **`"extend"`** (the default) adds your rules to that area's built-in rules.
  `"test": {"directory_prefixes": ["it_"]}` keeps every existing test rule and
  adds one.
- **`"replace"`** discards that area's built-in rules and uses only yours. It
  changes the rules, not what kind of work the area is: an area stays code-like
  unless the block says `"code_like"` explicitly.

Either way, a name the built-ins do not know is a new area, and new areas are
matched **before** the built-ins (among themselves in name order). That is what
makes `.claude/settings.json` land in `ai` rather than `config`.

Every rule set is a list of plain strings — no regular expressions:

| Rule | Matches |
| --- | --- |
| `directories` | An exact path component above the file name (`"corpus"`, `".claude"`). |
| `directory_prefixes` / `directory_suffixes` | A path component starting or ending with it (`"for_"`, `".specs"`). |
| `extensions` | The final extension, written `".rs"` or `"rs"`. |
| `names` | The whole file name (`"CLAUDE.md"`). |
| `name_prefixes` / `name_suffixes` / `name_contains` | Part of the file name (`"when_"`, `".test."`). |
| `stems` | The file name without its final extension (`"readme"`). |
| `stem_suffixes` | The end of that stem (`"_test"`). |
| `cased_stem_suffixes` | The same, matched against the **original** casing, so `UserTest.cs` is a test and `Latest.cs` is not. |
| `globs` | A glob over the whole path, matched case-sensitively (`"docs/**/*.png"`). |
| `code_like` | `true` opts the area into the `new code` / `revision` / `removal` shapes instead of being named directly. |

Everything except `cased_stem_suffixes` and `globs` is case-insensitive, so
`"CLAUDE.md"` and `"claude.md"` behave the same. Within one area the rule kinds
are tried most specific first — globs, then directories, then file names, then
the extension — which only decides *which rule* is reported as the reason, never
which area wins.

The registry is bounded the way the source-root rules are: at most 32
categories, 128 rules per category, 128 bytes per rule (256 for a glob), no
empty strings and no control characters. A category name must be lowercase
`[a-z][a-z0-9_-]*`, at most 32 characters; `ignored` is reserved, because
`ignored_additions` is already a CSV column of its own. Breaking one of those
bounds, or writing a `category_mode` other than `extend`/`replace`, stops the
run with a message naming the problem rather than quietly reporting different
numbers. A misspelled *rule key* is a JSON error like any other malformation:
the whole config is ignored for that run and the reason is printed as a
`Warning:` line under the report, so a typo is visible instead of silently
partial.

`workstats classify` answers "why did this file land there?" without running a
report:

```bash
$ workstats classify src/main.rs docs/design.md .claude/settings.json
PATH                                                 CATEGORY   RULE               MATCHED
src/main.rs                                          source     extension          rs
docs/design.md                                       docs       directory          docs
.claude/settings.json                                ai         directory          .claude

Categories in match order: ai, test, docs, config, assets, source, other
```

It reads the same config the report does (`--config PATH` to point elsewhere)
and supports `--format json` and `--format csv`.
