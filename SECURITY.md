# Security

## Reporting a vulnerability

Please use the repository's **Security → Report a vulnerability** form so a
report and any proof of concept remain private until a fix is available. Do not
open a public issue for a vulnerability that could expose transcript metadata or
local filesystem information.

## Data boundary

`workstats` reads local Git history and structural metadata from locally retained
Codex and Claude Code transcripts. It does not make network requests, deserialize
message bodies into the reporting model, or place prompt/response text in its
cache. JSON and CSV reports can contain repository names and working-directory
paths; review them before sharing.
