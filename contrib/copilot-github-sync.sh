#!/usr/bin/env bash
#
# copilot-github-sync.sh — record GitHub-side Copilot activity as workstats events.
#
# WHY THIS LIVES OUTSIDE THE BINARY
#
#   Two things Copilot does on github.com leave no trace in a clone: pull requests
#   the coding agent opened, and code reviews it left. The default refspec fetches
#   `refs/heads/*` only, so `refs/pull/*` is never on disk, and a review suggestion
#   the author declined leaves no artifact at all. Reading either one means calling
#   the GitHub API.
#
#   `workstats` promises two things an API client inside it would break. It makes
#   no network calls except `workstats update`, and it performs no credential
#   discovery — it never reads a keyring, a token file, or GH_TOKEN. An HTTP client
#   in the binary would end both promises, and the second is the expensive one:
#   from then on the tool would have to be audited for how it holds a token, how it
#   keeps one out of `--format json`, and what a poisoned cache could do with one.
#
#   So the network call is made out here, by the user's own `gh`, with the user's
#   own credentials, only when the user runs this script. What crosses back into
#   `workstats` is the content-free record the `record` subcommand exists for: a
#   provider, an identifier, a directory, a model name, and timestamps. No titles,
#   no bodies, no review text — the events-v1 schema refuses records carrying any
#   of those, so a careless change here cannot leak a pull request body into the
#   log.
#
#   Every event is written with `--role subagent`. That is the same lever the
#   in-process adapters use: a subagent contributes to AI wall clock and to session
#   counts, and contributes exactly zero to the human-work estimate. Nothing this
#   script records can move the number the report exists to keep honest.
#
# WHAT IS ALREADY COVERED WITHOUT THIS SCRIPT
#
#   Commits the coding agent authored are ordinary local history once a branch has
#   been fetched, and `workstats --agent-commits` reads them with no network at
#   all. This script is for what a clone cannot see. Note that a squash-merged
#   agent pull request is visible both ways: as an agent-authored commit and as an
#   event recorded here. Both are `subagent`, so neither can become human time, but
#   AI wall clock would count the work twice. Use one or the other per repository.
#
# USAGE
#
#   contrib/copilot-github-sync.sh [options] <clone-path> [clone-path ...]
#
#   Each argument is a local clone. The repository slug comes from its `origin`
#   remote and the clone's own path becomes the event's `cwd`, which is how these
#   events land on the same report row as the rest of that repository's work.
#
#     --since YYYY-MM-DD   only activity created on or after this date
#     --jsonl FILE         append events-v1 JSONL directly instead of running
#                          `workstats record` once per event; much faster for a
#                          first backfill of several hundred pull requests, and
#                          read back with `workstats --events FILE`
#     --workstats PATH     the workstats binary to use (default: workstats on PATH)
#     --dry-run            print what would be recorded and record nothing
#     -h, --help           this text
#
# REQUIREMENTS
#
#   `gh`, authenticated (`gh auth status`), with `repo` scope for private
#   repositories or `public_repo` for public ones. No `jq`: every filter below runs
#   through `gh --jq`, which is built in.
#
# EXIT STATUS
#
#   0 when every clone named on the command line was read. Anything that could
#   not be — not a clone, no github.com remote, a repository this account cannot
#   search, a pull request whose reviews would not load — is named on stderr,
#   skipped, and the remaining clones are still done; the run then exits 1. The
#   events that did not arrive are invisible in a report by definition, so a
#   partial sync has to be loud about it or a scheduled run looks like a clean
#   one.
#
# RATE LIMITS
#
#   The search API allows 30 requests per minute; the rest of the REST API allows
#   5000 per hour. The search calls are paced accordingly, which is why a wide
#   backfill takes minutes rather than seconds. Search also stops at 1000 results
#   per query, so a repository with more agent pull requests than that needs
#   `--since` to be narrowed and the script run more than once.

set -euo pipefail

# GitHub App slugs, taken from working API queries rather than guessed. Both
# render under the login `Copilot`, but `author:` and `reviewed-by:` match the app
# slug, and the two are different apps.
readonly CODING_AGENT_APP="copilot-swe-agent"
readonly REVIEWER_APP="copilot-pull-request-reviewer"
readonly REVIEWER_LOGIN="copilot-pull-request-reviewer[bot]"

# A second between search calls stays well inside 30/min even when a page is
# served instantly from cache.
readonly SEARCH_PACE_SECONDS=1

usage() {
    cat <<'TEXT'
usage: copilot-github-sync.sh [options] <clone-path> [clone-path ...]

  --since YYYY-MM-DD   only activity created on or after this date
  --jsonl FILE         append events-v1 JSONL instead of calling `workstats record`
  --workstats PATH     the workstats binary to use (default: workstats on PATH)
  --dry-run            print what would be recorded and record nothing
  -h, --help           this text

Every event is recorded with role=subagent and can never become human time.
See the comment at the top of this file for why it is a script and not a flag.
TEXT
}

since=""
jsonl=""
workstats_bin="workstats"
dry_run=0
paths=()

while [ $# -gt 0 ]; do
    case "$1" in
        --since) since="${2:?--since needs a date}"; shift 2 ;;
        --jsonl) jsonl="${2:?--jsonl needs a file}"; shift 2 ;;
        --workstats) workstats_bin="${2:?--workstats needs a path}"; shift 2 ;;
        --dry-run) dry_run=1; shift ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
        *) paths+=("$1"); shift ;;
    esac
done

# Everything this script says about its own progress goes to stderr, so that
# `--dry-run` output and nothing else is on stdout.
warn() { echo "$*" >&2; }

if [ "${#paths[@]}" -eq 0 ]; then
    usage >&2
    exit 2
fi

# Checked here rather than left to GitHub, which answers a malformed date with a
# 422 raised from inside a paginated search, naming neither the flag nor the
# value that caused it.
case "$since" in
    '' | [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]) ;;
    *) warn "--since takes a date written YYYY-MM-DD, not: $since"; exit 2 ;;
esac

command -v gh >/dev/null || { warn "gh is required: https://cli.github.com"; exit 1; }
gh auth status >/dev/null 2>&1 || { warn "gh is not authenticated; run: gh auth login"; exit 1; }
if [ -z "$jsonl" ] && [ "$dry_run" -eq 0 ] && ! command -v "$workstats_bin" >/dev/null; then
    warn "workstats not found on PATH; pass --workstats PATH"
    exit 1
fi

# Set when a repository could not be read, so that a run over several clones
# finishes the ones it can and still exits non-zero. A silent partial sync would
# be worse than a loud one: the events that are missing are invisible by
# definition, and the next report would simply be quietly short.
failures=0

# JSON string escaping for the `--jsonl` fast path, which is the one place this
# script writes JSON by hand — `workstats record` builds its own. A clone's path
# and its slug are the only fields carrying arbitrary text, and a directory name
# may legitimately hold a backslash or a quote; an unescaped one would make the
# whole line unreadable to `--events`.
json_escape() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//\"/\\\"}"
    printf '%s' "$value"
}

# owner/repo from the clone's own remote, so the slug and the `cwd` can never end
# up describing two different repositories.
slug_of() {
    local url slug
    url="$(git -C "$1" config --get remote.origin.url 2>/dev/null || true)"
    [ -n "$url" ] || return 1
    slug="$(printf '%s' "${url%.git}" | sed -E 's#^.*github\.com[:/]##')"
    case "$slug" in
        */*) printf '%s' "$slug" ;;
        *) return 1 ;;
    esac
}

# One content-free event. `--started-at`/`--completed-at` are passed only when the
# interval is genuinely positive: `workstats record` refuses an end that is not
# after its start, and a pull request opened and closed in the same second is a
# real thing that happens.
record_event() {
    local provider="$1" session="$2" model="$3" cwd="$4" started="$5" completed="$6"
    if [ "$dry_run" -eq 1 ]; then
        printf '%s\t%s\t%s\t%s..%s\n' "$provider" "$session" "$model" "$started" "${completed:-—}"
        return 0
    fi
    if [ -n "$jsonl" ]; then
        # Schema-valid events-v1, written straight out. `record` forks once per
        # event, which is fine nightly and slow for a three-hundred-PR backfill.
        # Only the session id and the directory can hold anything but a fixed
        # vocabulary, so those two are escaped; the rest are timestamps and app
        # slugs this script chose itself.
        local safe_session safe_cwd
        safe_session="$(json_escape "$session")"
        safe_cwd="$(json_escape "$cwd")"
        if [ -n "$completed" ] && [ "$completed" != "$started" ]; then
            printf '{"timestamp":"%s","provider":"%s","session_id":"%s","cwd":"%s","model":"%s","event":"activity","role":"subagent","started_at":"%s","completed_at":"%s"}\n' \
                "$completed" "$provider" "$safe_session" "$safe_cwd" "$model" "$started" "$completed" >>"$jsonl"
        else
            printf '{"timestamp":"%s","provider":"%s","session_id":"%s","cwd":"%s","model":"%s","event":"activity","role":"subagent"}\n' \
                "$started" "$provider" "$safe_session" "$safe_cwd" "$model" >>"$jsonl"
        fi
        return 0
    fi
    local arguments reason
    arguments=(record --provider "$provider" --session "$session" --model "$model"
               --cwd "$cwd" --kind activity --role subagent)
    if [ -n "$completed" ] && [ "$completed" != "$started" ]; then
        arguments+=(--started-at "$started" --completed-at "$completed")
    else
        arguments+=(--timestamp "$started")
    fi
    # Carrying the reason back matters: `record` refuses a malformed timestamp
    # and a record it considers privacy-bearing in the same breath, and those
    # want opposite responses from whoever is reading this output.
    if ! reason="$("$workstats_bin" "${arguments[@]}" 2>&1 >/dev/null)"; then
        warn "workstats record refused $session: ${reason:-no reason given}"
    fi
    return 0
}

# Pull requests the coding agent opened. `created_at` is when its work surfaced
# and `closed_at` when the pull request stopped moving — neither is the agent's
# own runtime, which GitHub does not publish anywhere this script could read it.
# Treat the interval as an outer bound on the work, not a measurement of it.
sync_agent_pull_requests() {
    local slug="$1" cwd="$2" query found
    query="repo:$slug is:pr author:app/$CODING_AGENT_APP"
    [ -n "$since" ] && query="$query created:>=$since"
    sleep "$SEARCH_PACE_SECONDS"
    # Collected before it is read, rather than piped into the loop, so that a
    # search this account cannot run is caught here and named. Piped, the
    # failure surfaced as `gh`'s bare HTTP status and — under `set -e` — took
    # the remaining clones down with it.
    if ! found="$(gh api -X GET search/issues --paginate -f per_page=100 -f q="$query" \
        --jq '.items[] | [(.number|tostring), .created_at, (.closed_at // .updated_at)] | @tsv')"; then
        warn "could not search $slug for agent pull requests; skipping them"
        return 1
    fi
    while IFS=$'\t' read -r number created closed; do
        [ -n "$number" ] || continue
        record_event "copilot-agent" "$slug#$number" "$CODING_AGENT_APP" "$cwd" "$created" "$closed"
    done <<EOF
$found
EOF
}

# Copilot code review. The search says only *which* pull requests it reviewed, so
# each is asked for its reviews to learn when each was submitted. A review is a
# point in time: how long it took is not published, and inventing a duration for
# it would be the sort of guess this tool refuses to make.
sync_agent_reviews() {
    local slug="$1" cwd="$2" query found reviews status=0
    query="repo:$slug is:pr reviewed-by:app/$REVIEWER_APP"
    [ -n "$since" ] && query="$query created:>=$since"
    sleep "$SEARCH_PACE_SECONDS"
    if ! found="$(gh api -X GET search/issues --paginate -f per_page=100 -f q="$query" \
        --jq '.items[].number')"; then
        warn "could not search $slug for Copilot reviews; skipping them"
        return 1
    fi
    while read -r number; do
        [ -n "$number" ] || continue
        # One unreadable pull request costs its own reviews and nothing else.
        if ! reviews="$(gh api "repos/$slug/pulls/$number/reviews" --paginate \
            --jq ".[] | select(.user.login == \"$REVIEWER_LOGIN\") | .submitted_at")"; then
            warn "could not read reviews on $slug#$number; skipping it"
            status=1
            continue
        fi
        while read -r submitted; do
            [ -n "$submitted" ] || continue
            record_event "copilot-review" "$slug#$number@$submitted" "$REVIEWER_APP" \
                "$cwd" "$submitted" ""
        done <<EOF
$reviews
EOF
    done <<EOF
$found
EOF
    return "$status"
}

for path in "${paths[@]}"; do
    if [ ! -e "$path/.git" ]; then
        warn "not a git clone, skipping: $path"
        failures=1
        continue
    fi
    absolute="$(cd "$path" && pwd)"
    if ! slug="$(slug_of "$absolute")"; then
        warn "no github.com origin remote, skipping: $path"
        failures=1
        continue
    fi
    warn "· $slug → $absolute"
    # Each clone is attempted whatever happened to the one before it; `|| ...`
    # is also what keeps `set -e` from ending the run on the first repository
    # this account cannot read.
    sync_agent_pull_requests "$slug" "$absolute" || failures=1
    sync_agent_reviews "$slug" "$absolute" || failures=1
done

if [ "$dry_run" -eq 1 ]; then
    warn "dry run — nothing was recorded"
elif [ -n "$jsonl" ]; then
    warn "wrote events to $jsonl — read them with: workstats --events $jsonl"
else
    warn "done — these events are in the log workstats already reads"
fi

# Non-zero when anything was skipped. The events that did not arrive are
# invisible in the report by definition, so a partial sync has to say so here
# or a scheduled run would look like a clean one.
if [ "$failures" -ne 0 ]; then
    warn "finished with skipped repositories or pull requests — see the warnings above"
    exit 1
fi
