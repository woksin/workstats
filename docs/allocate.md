# Splitting the bill

Allocating cost across people and projects with `workstats allocate`.

You pay for the plans; the work is spread across projects. `allocate` answers
what share of that spend one project accounts for.

```console
$ workstats allocate -p Ada --sub claude=2 --sub codex=4 --month 2026-08

  ALLOCATION  Ada
  2026-08 · 6 subscriptions @ $200/mo · $1,200 billed · basis: output tokens

  month     family   subs      project         pool    share       owed
  ────────────────────────────────────────────────────────────────────
  2026-08   claude      2        65.5M       147.5M    44.4%       $178
  2026-08   openai      4        45.8M       112.1M    40.8%       $326
  ────────────────────────────────────────────────────────────────────
  ATTRIBUTABLE                                        42.0%       $504
```

Each vendor's pool is split on its own, then weighted by how many plans you
hold there — Ada took 44% of the Claude pool and 41% of the OpenAI one, and
those are worth different amounts because the plan counts differ. Pooling every
token together would have said 42.9%.

Under the summary, every model that ran is listed with its project and pool
tokens and its list-price value, because a share nobody can audit is just an
assertion.

### Leave the project out to see where everything went

`-p` names the project a claim is being made for. Omit it and no claim is being
made, so the question becomes how the whole spend divided:

```console
$ workstats allocate --sub claude=2@1866 --sub codex=3@1992 \
    --currency NOK --vat 25 --month 2026-08 --top 6

  ALLOCATION  every project
  2026-08 · 5 subscriptions · 12,135 kr billed incl. 25% tax · basis: output tokens

  PROJECTS
  project                              claude       openai        TOTAL       %        out    tokens
  ──────────────────────────────────────────────────────────────────────────────────────────────────
  Ada                                2,072 kr     3,048 kr     5,121 kr   42.2%    111.3M   38.28B
  Chronicle.Wolverine [f81e3bee]       102 kr     1,020 kr     1,121 kr    9.2%     18.5M    7.56B
  AI                                   295 kr       433 kr       728 kr    6.0%     15.8M    7.41B
  cratis                               385 kr       261 kr       646 kr    5.3%     16.1M    5.89B
  Screenplay                            83 kr       532 kr       616 kr    5.1%     10.6M    5.07B
  Strategy                              90 kr       491 kr       581 kr    4.8%     10.2M    5.55B
  (59 smaller projects)                                        3,322 kr   27.4%
```

One column per plan you hold, so a project that leans on one vendor is obvious.
The rows reconcile exactly to what was billed. `--top` bounds the list and the
remainder is stated rather than dropped; `--top 0` shows every project.

### Pick the measure, then check it against the others

`--basis` chooses what the split is computed from: `output` tokens (default),
`value` at list rates, `wall` clock, `tokens` in total, or `human` time. Every
run prints all five:

```
  CROSS-CHECK  same window, every basis
   ~ human time (est)       51.6%   $      620
     agent wall clock       43.4%   $      520
     output tokens          42.0%   $      504  ←
     list-price value       38.8%   $      466
     total tokens           36.5%   $      438
     ~ estimated, not provider-recorded
```

A number you are going to hand someone else should not depend on a metric you
picked quietly. Where these agree, the share is solid; where they diverge, the
divergence is the finding.

Output tokens are the default because they are recorded by the provider and are
not swamped by cache reads, which track context length and turn count rather
than work. Human time is *not* the default and is marked estimated: it is
inferred from prompt counts and session edges, so a run that fans out subagents
books more apparent attention per real hour than one long session does.

Models are weighed by published list rate, so a million Opus tokens and a
million Haiku tokens are not equal claims on a plan. List value is a ceiling
and a weighting — it is what the usage would have cost per-token, which is
precisely what a subscription holder does not pay.

### Rates, and when they go stale

The list rates behind the `value` basis and the per-model list values are a
table compiled into the binary. The table's date is printed on the `MODELS`
line (`rates as of 2026-09-02`), and once it is more than 90 days old against
today's date, `allocate` adds a warning saying how old it is:

```
  WARNINGS
   ! built-in list rates are dated 2026-09-02 (121 days old, past the 90-day
     limit) and vendors may have repriced since; set current prices under
     "model_rates" in the config file, or upgrade workstats for a refreshed table
```

Only `allocate` uses rates; ordinary reports carry no list value, so they never
show this warning. It is also left out when every priced model in the run took
its rate from your own `model_rates`, since nothing built-in was relied on.

To correct a price, or to price a model the table has never heard of (which is
otherwise `← unpriced` and drops out of the `value` basis), add `model_rates`
to the config file. Rates are USD per million tokens, like the built-in table,
and all four are required:

```json
{
  "model_rates": {
    "claude-opus-5": { "input": 5, "cache_write": 6.25, "cache_read": 0.5, "output": 25 },
    "acme-coder":    { "input": 1, "cache_write": 1.25, "cache_read": 0.1, "output": 8,
                       "family": "openai" }
  }
}
```

- **Matching** is the built-in table's: the key is a model-name prefix, compared
  case-insensitively with `.` and `-` treated alike, and the longest matching
  key wins. `claude-opus-5` therefore also prices `claude-opus-5-20260101`, and
  `gpt-5.5-pro` beats `gpt-5.5` for a Pro model. There are no wildcards.
- **Precedence**: any matching override beats the built-in table, whatever the
  length of the built-in prefix. Models no override matches keep their built-in
  rate.
- **`family`** (optional) is the subscription pool the model draws on: `claude`,
  `openai`, `google`, or an alias such as `codex`. Without it the model keeps
  its built-in family, or else the one its name implies (`claude-*`, `gpt-*`,
  `gemini-*`). Set it for a model whose name says nothing, or it is excluded
  from every pool. Clients that bill on their own seat, such as Copilot, stay
  in their own pool regardless.
- **Validation** happens before anything is scanned. A negative or non-numeric
  rate, a missing rate, an unknown `family`, a blank key, or two keys that match
  the same models (`gpt-5.5` and `gpt-5-5`) stops the run with an error naming
  the key (a misspelt field such as `inptu`, or a value of the wrong type, is
  refused as `invalid model_rates.<key>` the same way), for example
  `invalid "model_rates" configuration: model rate
  "acme-coder": "output" must be a non-negative number of USD per million
  tokens, got -3`.

When an override priced a model in the run, the output says so beside the
rates-as-of line, so the numbers can be audited:

```
  MODELS  (rates as of 2026-09-02; overridden by model_rates: acme-coder)
  model                      family    project        pool    share   list value
  ──────────────────────────────────────────────────────────────────────────────
  acme-coder-1               openai       1.0M        10.0M    10.0%         $8  ← override
```

`--format json` carries the same facts as `rate_overrides` (the keys that were
applied) and a `rate_source` of `override`, `built-in`, or `none` on each model.
Overrides change only the weighting and the stated list values, never a
measured quantity.

### Missing history is not zero usage

Retention prunes old transcripts. A month that has been pruned looks exactly
like a month of no work, and reporting it as 0% quietly moves that spend onto
you:

```
  2026-07   openai      4            0            0     0.0%         $0  ← excluded
  ────────────────────────────────────────────────────────────────────
  ATTRIBUTABLE                                        42.8%       $685
  of $1,600 documented — $800 of $2,400 billed has no surviving history

  WARNINGS
   ! no openai history for 2026-07 (history is pruned, not idle) — $800
     excluded from both the claim and documented spend
```

`--gap-policy` decides what happens, and never decides it silently:

| Policy | Effect |
| --- | --- |
| `skip` (default) | Drops the gap from the claim *and* from documented spend, so the share stays honest about what it saw |
| `zero` | Claims nothing but still counts the spend — the most conservative number available |
| `impute` | Applies that vendor's mean share from the months it can see |

### Currency, tax, and two vendors who charge differently

`--price` is the advertised price before tax; `--vat` adds what checkout adds;
`--currency` says which currency you are stating, and converts nothing.

Vendors outside your own country rarely cost the same amount. Anthropic prices
in dollars everywhere and your card converts them; OpenAI sets a local price.
`--sub PLAN=N@PRICE` prices one vendor apart from the rest:

```console
$ workstats allocate -p Ada --sub claude=2@1866 --sub codex=3@1992 \
    --currency NOK --vat 25 --month 2026-08

  2026-08 · 5 subscriptions · 12,135 kr billed incl. 25% tax · basis: output tokens

  month     family   subs    plan/mo      project         pool    share        owed
  ─────────────────────────────────────────────────────────────────────────────────
  2026-08   claude      2   2,333 kr        65.5M       147.5M    44.4%    2,072 kr
  2026-08   openai      3   2,490 kr        45.8M       112.1M    40.8%    3,048 kr
  ─────────────────────────────────────────────────────────────────────────────────
  ATTRIBUTABLE                                                    42.2%    5,121 kr
```

The tax-inclusive unit price sits on each row, so a mixed-vendor claim can be
checked line by line against a bank statement.

No exchange rate is ever applied. workstats makes no network calls outside
`workstats update`, and a rate compiled into a binary goes stale without saying
so — the last thing you want behind a number you are about to invoice. State
the amount you were actually charged and the arithmetic stays yours.

### Clients that bill on their own seat

A Copilot seat is not a Claude or ChatGPT plan, even when it runs their models.
Copilot forms its own pool rather than inflating a vendor pool it was never
billed to — which also keeps it from masking a month where the vendor's own
history is gone. Declare it like any other plan with `--sub copilot=1`.

Pools with no plan declared take no part in the split at all.
