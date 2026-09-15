# MCP conformance baseline

WebCodex tracks MCP wire compatibility with the upstream
[`modelcontextprotocol/conformance`](https://github.com/modelcontextprotocol/conformance)
referee in addition to its focused Rust tests. The baseline is evidence about the
checks that actually ran; it is not a blanket "MCP compliant" badge.

## Pinned referee and profiles

`scripts/mcp_conformance.sh` pins the upstream referee to the immutable commit in
`tests/fixtures/mcp/conformance/baseline.json`. The script rejects any checkout at
a different commit and installs dependencies from that checkout's lockfile before
building it. Updating the pin is therefore a reviewed test-contract change, not a
floating dependency update.

The ordinary baseline runs two dated server profiles independently:

- `2026-07-28`, using the stateless per-request protocol and that revision's
  frozen requirements;
- `2025-11-25`, using the stateful lifecycle and its reconstructed frozen
  requirements.

Focused repository tests continue to cover `2025-06-18`; the conformance harness
is not used to invent a frozen requirements set that it does not publish.

Run the same baseline locally with:

```bash
bash scripts/mcp_conformance.sh
```

To reuse an already checked-out referee without changing the pin:

```bash
WEBCODEX_MCP_CONFORMANCE_HARNESS_DIR=/path/to/conformance \
  bash scripts/mcp_conformance.sh 2026-07-28
```

The supplied checkout must resolve to the exact pinned commit. Reports are written
to `target/mcp-conformance/reports` by default. CI uploads that directory even
when the gate fails so the raw evidence remains inspectable.

## What is exercised

The script starts one ignored, test-only Rust fixture on `127.0.0.1:0`. The
fixture composes the same Salvo `AuthMiddleware` and `/mcp` handlers used by the
production HTTP surface, with temporary database/runtime state and anonymous test
authority. The referee therefore reaches real WebCodex parsing, dispatch and
rendering without production credentials or an external deployment.

No conformance-only tool is added to the production registry. Some upstream
scenarios intentionally expect named reference-fixture tools. When WebCodex does
not expose such a tool, that result is a coverage gap until a test-only adapter is
provided; it must not be relabeled as a product protocol failure merely to make a
summary green.

Authenticated Runtime/Connector behavior remains covered by focused synthetic-
credential integration tests. The upstream server CLI does not provide a generic
way to inject WebCodex authorization headers, so an authentication-blocked
scenario is inconclusive rather than a pass.

## Coverage-aware report gate

The upstream runner saves one `checks.json` file per scenario. After each profile,
`scripts/mcp_conformance_report.py` verifies the raw reports against the exact
required server-scenario list and the reviewed baseline.

Every `FAILURE`, `WARNING`, or `SKIPPED` check must have an exact
`<scenario>, <check_id>` classification in
`tests/fixtures/mcp/conformance/baseline.json`. Whole-scenario wildcards are
rejected because they can hide a newly failing check behind an unrelated known
failure. Supported classifications are:

| Classification | Meaning |
|---|---|
| `genuine_protocol_failure` | The intended WebCodex behavior was reached and a protocol requirement is known to be violated. |
| `missing_harness_fixture` | The upstream scenario depends on reference-fixture behavior that the product endpoint does not expose. The product behavior remains untested by that check. |
| `optional_capability_not_implemented` | A capability is genuinely optional and is not advertised/implemented. The entry must cite capability/spec evidence. |
| `harness_limitation_pending` | The pinned referee itself marks or demonstrates a limitation/pending check. |
| `inconclusive_infrastructure` | Authentication blocking, timeout, harness exception, or other infrastructure failure. This is recorded but **never satisfies the CI gate**. |

The gate also fails when:

- no applicable `SUCCESS`/`FAILURE`/`WARNING` checks were emitted;
- a required scenario report is missing, duplicated, or unexpectedly present;
- a new non-success check has no exact classification;
- a classified check is no longer emitted or now passes, making its baseline
  entry stale;
- an inconclusive infrastructure result is present.

This is intentionally stricter than trusting the referee process exit code. A
zero exit can coexist with skipped/unscored coverage, and an expected-failure run
can intentionally tolerate known protocol defects.

The report gate's own regression suite is dependency-free:

```bash
python3 scripts/tests/test_mcp_conformance_report.py
```

It covers all-skipped output, missing required scenarios, harness errors, new
failures, stale classifications, and rejection of broad masks.

## Raw evidence and review policy

Each profile retains:

- the referee's raw per-scenario `checks.json` files;
- the referee stdout/stderr log;
- metadata containing the WebCodex source SHA, referee SHA, profile, upstream
  referee exit code, and exact required scenario list;
- the coverage-aware WebCodex summary.

Baseline updates should be narrow. Add a classification only after reproducing
and understanding the exact check. Remove it as soon as that check passes. Do not
replace multiple check IDs with a scenario-wide exception.

This baseline deliberately records two already-demonstrated 2026 result-shape findings
without fixing production behavior: streamed artifact `resources/read` omits the
cache hints emitted by ordinary resource reads, and a completed Connector Task
embeds its original tool result without a nested `resultType`. Their focused Rust
observations and planned follow-up are recorded in the baseline fixture. Fixes
belong in a separate protocol-behavior change so this baseline remains an
independent measuring instrument.
