# Handoff — `.github/workflows/release.yml` hardening

Only file touched: `/Volumes/sourcecode/repos/woksin/workstats/.github/workflows/release.yml`.
No other file was read-modified, no cargo, no git commands beyond `git status --porcelain -- .github/`,
no workflow triggered or re-run.

All three changes are confined to the `homebrew` job (`Update the Homebrew tap`). Everything the
formula *contains* is byte-for-byte unchanged — verified by reproduction, see below.

---

## 1. The silent checksum bug (spec §4 "The real silent-failure risk", §6.2)

**Step: `Write the formula`.**

- `sha()` rewritten as a multi-line function that:
  - fails the step with `missing artifact: <path>` when the tarball is absent (`[ ! -f ]`);
  - prefers `sha256sum` and falls back to `shasum -a 256`, guarded with `command -v` — the same
    either/or the `Package` step in the `binaries` job already uses (spec §4 "Secondary risk");
  - validates the result against `^[0-9a-f]{64}$` and fails on anything else.
- The four digests are now hoisted to **statement-level assignments** before the heredoc:
  `sha_macos_arm`, `sha_macos_x86`, `sha_linux_arm`, `sha_linux_x86`. `errexit` applies to an
  assignment; it does not apply to a `$(…)` buried in a heredoc body, which is what allowed
  `sha256 ""` to ship green.
- The heredoc now interpolates `${sha_macos_arm}` etc. instead of `$(sha macos-arm64)`.
- **Post-generation assertions** added after the heredoc, before `cat`:
  - `grep -q 'sha256 ""'` → fail;
  - `grep -c '^      sha256 "'` must equal `4` → fail otherwise (catches a field lost entirely,
    not just emptied).

Naming note: lower-case shell locals (`sha_macos_arm`) rather than the spec's upper-case
`SHA_MACOS_ARM`, to keep upper-case reserved for the job's `env:` values (`TAP_KEY`, `VERSION`,
`TAG`), which is the convention the rest of the file already follows.

## 2. The guard that hid the original failure (spec §6.1)

**New first step:** `actions/checkout@v7` with `sparse-checkout: README.md` and
`sparse-checkout-cone-mode: false`. The job previously had no working tree at all; this is the only
reason it needs one.

**Step `Is the tap configured?`** now has `set -euo pipefail` and three outcomes instead of two:

| `TAP_KEY` | README advertises a tap | Result |
|---|---|---|
| set | either | `configured=true`, as before |
| empty | yes | `::error title=Homebrew tap advertised but not configured::…` + `exit 1` — **the release fails** |
| empty | no | `::warning title=Homebrew tap skipped::…` **plus** the original step-summary line, `configured=false`, job continues green |

Detection: `[ -f README.md ] && grep -qE 'brew (tap|install) woksin/' README.md`. That single pattern
covers both spellings the spec discusses — the fully-qualified one-liner
`brew install woksin/workstats/workstats` and the older `brew tap woksin/workstats` pair. The
`[ -f ]` guard means a missing README degrades to "not advertised" rather than to grep noise.

The warning annotation is the point: it surfaces in the run header and the Actions list, unlike the
step-summary line that hid this for every release up to v1.0.2.

## 3. Read-back verification (spec §6.3)

**New final step `Verify the published formula`**, same `if: steps.guard.outputs.configured == 'true'`
guard, `GH_TOKEN: ${{ github.token }}`:

- `gh api "repos/${TAP_REPO}/contents/Formula/workstats.rb" --jq '.content' | base64 -d`
- Up to 3 attempts with `sleep 5` between them (see assumptions), breaking as soon as the content
  matches.
- Fails unless the fetched file contains `version "${VERSION}"`.
- Fails if the fetched file contains `sha256 ""`.
- On success writes `Tap published workstats <version>.` to `$GITHUB_STEP_SUMMARY`.

Note the `Push it to the tap` step can still `exit 0` early on "Formula already at X, nothing to
push" — verification runs regardless and passes, which is correct.

---

## What was actually exercised locally

Every `run:` script in the file was extracted from the YAML and `bash -n`-checked. The YAML itself
parses. Then, in `…/scratchpad/wfcheck/`:

- **Formula generation, happy path** — four dummy artifacts, `TAG=v1.0.2`, `VERSION=1.0.2`. Output is
  structurally identical to the byte-exact formula in spec §3 Step 6b (same lines, same indentation,
  same ordering; only the dummy digests differ). Exit 0.
- **Missing artifact** — `workstats-linux-arm64.tar.gz` removed. Exit **1**, `missing artifact: …` on
  stderr, `out/workstats.rb` **not written**. This is the exact case that previously exited 0 with
  `sha256 ""`.
- **Assertions** — a generated formula doctored to have one empty `sha256` field → exit 1
  ("empty sha256 field"); one field deleted outright → exit 1 ("expected 4 sha256 fields"); intact →
  exit 0.
- **Guard, all five branches** — advertised + secret → `configured=true`; advertised (one-liner) +
  no secret → `::error` + exit 1; advertised (`brew tap`) + no secret → `::error` + exit 1;
  not advertised + no secret → `::warning` + summary line + `configured=false`, exit 0;
  no README at all + no secret → same as not advertised.
- **Verify, four branches** — with `gh` and `sleep` stubbed on `PATH`: correct formula → exit 0 and
  the summary line; stale version (`1.0.1`) → exit 1; empty `sha256` → exit 1; `gh` failing outright
  → exit 1 after 3 attempts.

## Assumptions and things not verified

1. **The tap repository must be public.** The verify step reads it with the workflow's
   `GITHUB_TOKEN`, which is scoped to `woksin/workstats`. That token can read a *public*
   `woksin/homebrew-workstats`; against a private tap it would 404 and the step would fail the
   release. Spec §2.1 already requires public, so this is consistent — but it is now load-bearing.
2. **The retry loop is my addition, not the spec's.** Spec §6.3 reads once. I added 3 attempts with
   a 5 s gap because a single read immediately after a push can hit replication lag on the contents
   API, and a false failure on a release is expensive. Worst case on a genuine failure: ~10 s of
   added runtime. Remove the loop if you would rather match the spec literally.
3. **Not exercised on a real runner.** No workflow was triggered. Untested against live GitHub:
   `actions/checkout@v7`'s `sparse-checkout-cone-mode: false` behaviour, `gh api` on a cross-repo
   contents path with `github.token`, and GNU `base64 -d` on the newline-wrapped `.content` blob
   (the latter is exactly the incantation spec §3 Step 6 uses by hand, so it is well established).
4. **Local shell was bash 3.2 on macOS**, GitHub's is bash 5.x on Linux. Nothing used is 4.x-only:
   `local`, `[[ =~ ]]`, `command -v`, `printf`, C-style-free `for` over a literal list.
5. **The README grep is repo-specific** (`woksin/`), matching the hardcoded `TAP_REPO`. If the tap
   owner ever changes, both need changing together.
6. **Interaction with the concurrent README edit.** Another agent is restoring the Homebrew block to
   `README.md` (spec §7). Once that lands, the guard's "advertised" branch becomes live: the next
   release **will fail** unless `HOMEBREW_TAP_DEPLOY_KEY` exists on `woksin/workstats`. That is the
   intended design, and it is exactly the ordering warning in spec §7 ("do not merge this README
   change until the tap works"). Sequence the secret (spec §3 Steps 1–5) before or with the README
   change.
7. **`README.md:531–532`** still says the tap is "skipped gracefully when absent". That is now only
   half true — it is skipped loudly when absent *and* unadvertised, and fatal when advertised.
   Updating that prose is the README agent's call, not mine; I did not touch it.
