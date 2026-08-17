# Security

## Reporting a vulnerability

Please use the repository's **Security → Report a vulnerability** form so a
report and any proof of concept remain private until a fix is available. Do not
open a public issue for a vulnerability that could expose transcript metadata or
local filesystem information.

## Data boundary

`workstats` reads local Git history and structural metadata from supported local
AI histories or explicit Workstats Events logs. It does not discover provider
credentials, deserialize message bodies into the reporting model, or place
prompt/response text in its cache.

The only network requests `workstats` ever makes are for checking or
installing its own updates: an explicit `workstats update[--check]`, or a
throttled background check that requires opting in (`--check-updates`,
`WORKSTATS_CHECK_UPDATES`, or `check_updates` in the config file). Both talk
only to GitHub's public release API and asset CDN over HTTPS; `workstats
update` verifies the downloaded binary against the release's published
`SHA256SUMS` before replacing the running executable. A normal report-only run
makes no network requests unless one of those is explicitly enabled.

Native SQLite adapters open databases read-only and query only session/message
tables. Credential stores—including `auth.json`, `secrets.json`, `.env`, private
keys, and OS keychains—are never discovery targets. JSON and CSV reports can
contain repository names and working-directory paths; review them before
sharing.
