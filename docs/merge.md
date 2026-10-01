# Merging machines

Work spread over a laptop and a desktop is still one person's work. `workstats export` writes what one machine saw, and `--import` / `workstats merge` fold that into a report on another, so the figures cover everything you did.

```bash
# on each machine
workstats export --month 2026-03 --label laptop --output laptop.json

# anywhere
workstats merge laptop.json desktop.json --month 2026-03
workstats merge laptop.json desktop.json --with-local --month 2026-03   # plus this machine
workstats --import desktop.json --month 2026-03                          # a normal report plus a bundle
```

## A bundle is evidence, not a report

A report is a conclusion: it holds hours that were already computed from one machine's signals. Adding two of them would count an hour twice whenever both machines were busy in it, and a report cannot be taken apart again. A bundle holds the signals themselves, so the importing machine runs its own pipeline over the union and **human time is recomputed from all the merged signals, never summed**. An hour in which the laptop and the desktop each saw some prompts is one hour.

`workstats merge` therefore refuses report JSON (`a report is a conclusion; export a bundle with workstats export`) and anything else that is not a `workstats-bundle`.

## What a bundle contains

Format `workstats-bundle`, version 1, one JSON file:

- the machine (`id`, `label`), the export time and the workstats version;
- `person.authors`: the Git author patterns the export used;
- the window it was exported for, and the exporter's `human_idle`, `review_credit` and `gap_cap` (informational);
- the repositories, by key (below);
- per session: provider, an opaque session id (below), repository, the subdirectory relative to the repository root, whether it is a subagent, branch marks, pull-request numbers, and the same timestamps, models and token counts the transcript cache holds;
- per commit: repository, SHA, author time, line counts, per-category line tallies, whether an agent authored or assisted it, and its branch.

A session id is exported as a hash, never as the provider wrote it: the first 128 bits of the SHA-256 of the provider and the id, in hex. Providers build their ids from the history layout (Claude's include the project folder, which is the dash-encoded working directory; the events format and a Codex session that changed directory include the directory), so the raw id can hold a username or a client's name. The hash is the same on every machine, so a session in a synced history folder still matches; this machine's own sessions are hashed the same way when they are compared with a bundle's.

It never contains prompts, responses, commit subjects, session titles or absolute paths. Changed file paths are left out unless you pass `--include-paths`. See [Privacy](privacy.md#bundles). Bundles are plain JSON and are **not encrypted**; treat one like the history it came from.

### Repository keys

| Key | Meaning |
| --- | --- |
| `remote:github.com/acme/api` | The repository's fetch remote, normalised. The same on every machine, so work on it merges. |
| `project:acme` | A [project alias](configuration.md) on the exporting machine. It passes through unchanged. |
| `local:<machine id>:<label>` | A repository with no remote (or a remote that is a path on that disk). Only that machine knows it, so it stays separate. |

The importing machine's own `project_aliases` are applied to `remote:` keys exactly as they are to a local checkout of the same remote, so a product configured on this machine groups the other machine's work too.

## What merging does

- **Sessions** are matched by `(provider, session id)` (the hashed id, for this machine's own sessions too), against this machine's sessions and across bundles. A session seen twice keeps one identity and gains the union of its activity points, human prompts, exact intervals and token events (an event on both sides counts once; a genuinely repeated event on one side counts as often as it occurred). This is what makes a synced history folder merge cleanly.
- **Branch marks** from a bundle are sorted by time, with the one mark that starts the session first; a second start or a mark that repeats the branch before it is dropped, and the run says how many.
- **Commits** are matched by `(repository key, SHA)`. The local copy wins, so a commit this machine read from Git itself is never replaced by an exported one.
- **`local:` repositories are never matched across machines**, because nothing says that two `scratch` folders are the same repository. Each is reported separately and the run warns, naming them. If you merge a bundle that this very machine exported (`--import` of your own bundle), sessions still match, but commits in `local:` repositories cannot, and are counted twice; the same warning applies. Give those repositories a remote, or leave them out of one side.
- **Categories** a bundle uses that this configuration does not define are counted as `other`, with a note.
- **Settings:** this machine's `--human-idle`, `--review-credit` and `--gap-cap` decide the merged hours. If a bundle was exported with other values, a note says which.
- **Coverage:** a bundle exported for a narrower window than the report asks for is noted, so a missing machine-week does not read as a quiet one.

## Filters and flags on imports

`--repo`, `--repo-exact`, `--provider` / `--exclude-provider` and the window flags apply to imported work as they do to local work. `--repo` matches the repository label, its remote key (so `acme/api` works) or the subdirectory; imported work has no path on this machine.

Some choices were made at export time and cannot be redone here:

- `--author` (the exporter's `person.authors` stand in), `--co-authors` and `--agent-commits`.
- `--path` / `--path-exclude` and `--no-ignore`: bundles carry no file paths by default, so the run warns that the filters do not apply to imported commits.
- Without `--include-paths`, imported commits add no files to the file counts; a note says how many commits are affected. Everything else (hours, commits, lines, categories, tokens) matches the local report.

`--no-ai` and `--no-git` choose which *local* histories are read; they never drop an import, which you asked for by name. `workstats merge` without `--with-local` reads only the bundles.

## One person only

All bundles in a merge must have the same `person.authors` (compared case-insensitively). Otherwise the merge is refused with `team merges are not supported yet`: human time for several people must be computed per person and then added, which a union of signals does not do. If this machine's Git author differs from the bundles', the run warns.

## The machine file

`workstats export` keeps `machine.json` beside the config file (`WORKSTATS_CONFIG` or `--config` decide where): a random 128-bit id and the label. The id never changes once written; it only keeps one machine's `local:` keys apart from another's. The label is `--label`, else the one already stored, else `HOSTNAME`/`COMPUTERNAME`; with none of them, export asks for `--label`. A `machine.json` that cannot be read is an error, not a reason to invent a new id. Delete it deliberately if you want this machine to become a new one.

## Options

`workstats export [REPORT FLAGS] [--output FILE|-] [--label NAME] [--include-paths]`

Takes the usual report flags (window, `--repo`, `--provider`, `--author`, `--dir`, ...) and exports what they select. Without `--output` the file is `workstats-bundle-<label>.json` in the current directory; `-` writes the bundle to stdout. `export` refuses `--import`.

`workstats merge FILE... [REPORT FLAGS] [--with-local]`

A report over the bundles, in any report format. `--import FILE` (repeatable) does the same inside an ordinary report.
