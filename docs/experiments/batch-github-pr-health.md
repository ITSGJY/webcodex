# Multi-PR GitHub CI status: batch read versus one query per PR

Date: 2026-10-12. Same Windows machine, same GitHub repository,
same `gh` executable, one authenticated read-only CLI session.
The compared operations used matching open PR numbers and retrieved
`number`, `mergeStateStatus` and `statusCheckRollup`.

## Controlled observation

The prior repeated workflow ran one `gh pr view N` per selected PR.
The candidate uses one `gh pr list --state open --limit 60 --json ...`
and filters the returned structured snapshot by the same PR numbers.
The 12 selected PRs were 1058-1065 and 1067-1070.

| Path | Data retrieved | Total subprocess + network time |
| --- | --- | ---: |
| Twelve serial `gh pr view` calls | 12 selected PRs | **18,421 ms** |
| One structured `gh pr list` call | 22 open PRs, including the same 12 | **6,036 ms** |

The merge-state fields and counts of returned CI statuses agreed for every
one of the 12 selected PRs (0 mismatches). The batch route was **3.05x
faster**, despite returning 22 PRs instead of only 12. An earlier
independent batch call with the same fields took 6,387 ms.

This is a single real-GitHub comparison, not a confidence interval or
WebCodex/ChatGPT Server runtime A/B. GitHub status may change between the
two non-atomic observations. Changing the query time, PR count, network,
GitHub service response or check list size changes relative costs.

## Reusable read-only implementation

`python3 scripts/open_pr_health.py --repo yyjeqhc/webcodex --only-perf`
fetches up to 50 open PRs using **exactly one** `gh pr list`
subprocess call. It outputs a concise CI/merge Markdown table; filter to
specified numbers with `--numbers 1073,1074,1078` and enlarge the
bounded fetch window using `--limit 100`. A list at its limit
explicitly warns about possibly missing additional PRs.

The formatter keeps skipped/neutral, failed, pending and unobserved check
states distinct; unknown CI is never silently reported as successful.
The CLI validates the repository target, limits, JSON shape and unique PR
identities before showing status. No API tokens, auth stderr, raw PR bodies,
permissions, approval, CI dispatch, merge or deployments are exposed or
modified. No additional GitHub SDK, Cargo compilation or network call is
made by the tests.

The benefit is **avoiding repeat GitHub CLI/process/remote requests during
maintenance**, not faster GitHub itself, and not a claim about end-to-end
LLM response time. For failed checks requiring precise logs or individual
PR review, use the normal targeted `gh pr view`/`gh run view`
after the batch identifies the exact PR.
