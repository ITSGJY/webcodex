# Related read-only CLI checks: Host-observed latency

Date: 2026-10-12. The sample was run against one connected Windows
WebCodex Server/Runner, build 805fc51d2c61172532d4bd95d3cf9b5bc90dc181.

## Workload and variants

A fixed four-command Git inspection on the same unchanged repository:

1. Git porcelain status
2. Current HEAD commit SHA
3. Current branch
4. Git diff summary

The **serial** route awaited four separate native run_process calls.
The **parallel** route issued the same four native calls with Promise.all
inside one Host JavaScript cell. The **related-chain** route used one bounded
run_shell call containing the four already fixed Git checks, explicit output
markers, and a per-command nonzero-exit check. All three routes returned
**identical normalized results** in both comparisons.

Measured Host-visible elapsed time with Date.now() around the tool calls,
not just the Runner process duration. Two orders: serial, parallel, chain;
then chain, parallel, serial. No source changes, builds, deployments or
service restarts occurred during the sample.

## Results

| Route | First run | Reversed run | Mean |
| --- | ---: | ---: | ---: |
| Four sequential run_process calls | 8,587 ms | 9,343 ms | 8,965 ms |
| Four concurrent run_process calls | 8,601 ms | 9,010 ms | 8,806 ms |
| One bounded related run_shell chain | 2,882 ms | 2,780 ms | **2,831 ms** |

The related-chain route was **3.17x faster** than sequential native calls
and **3.11x faster** than concurrent native calls for this controlled task.
Parallel calls were not measurably faster here. Do not generalize the
parallel result to other Hosts, long-running workloads or Runner loads.

## Engineering decision and limits

Use existing structured native batches for read_files, search_project_texts,
compound search/read, and package-scoped project_validate. For **one** native
executable with literal argv, keep run_process. When several *predetermined,
related, read-only* native CLI inspections form one diagnostic and need shell
composition, a bounded run_shell chain with explicit individual exit checks
avoids repeated Host/tool handoffs.

Do not chain mutations, uncertain or replay-sensitive effects, authority
decisions, or result-dependent actions. No Job, Session, read revision, error,
authorization, rollback or retry contract changes are intended. Do not open a
persistent local shell merely to run related commands.

This is a measured execution pattern, **not proof that changing guidance
causes an autonomous model to select it**, nor a model-turn or inference
benchmark. Evaluating the new guidance as an automatic selection policy
requires independently validated paired model-task samples using the
existing Agent Loop benchmark protocol.
