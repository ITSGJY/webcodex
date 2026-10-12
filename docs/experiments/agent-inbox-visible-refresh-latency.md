# Visible Agent inbox: 30s to 10s read-refresh deadline

Date: 2026-10-12. Scope: the WebCodex Runtime V2 browser Agent workspace,
**not** the coding tool executor, model inference, Server/Runner, or an
end-to-end ChatGPT task.

## Problem

The existing Agent inbox fetch is triggered by the shared Agent/Conversation
inventory revision, which refreshes every 30 seconds while a browser tab
is visible. Messages delivered by other Agents after the latest fetch can
remain unseen until the next 30s tick. The fixed refresh schedule makes the
additional read-observation delay approximately 0–30s (15s mean under
uniform event phase), even when the user is viewing an attached Agent inbox.

## Candidate

Retain the original 30s Agent and Conversation inventory schedule, but add
one **10s visible-only, single-flight** read of the currently selected,
attached Agent inbox. The fast timer does not run without an Agent Endpoint
or while the browser page is hidden, does not alter the independent 30s
Endpoint lease renewal, and never retries a business effect. Replacing or
detaching the Endpoint still aborts and fences late responses.

This reduces the *polling component* of the visible inbox's theoretical
worst-case new-delivery display delay from **30s to 10s**, or by up to
**20 seconds**, and its uniform-phase mean from 15s to 5s. The fixed 10s
bound excludes actual request latency, response failures, rate limits,
browser timer throttling, sleeping tabs, and backend delivery latency.
It does **not** promise an end-to-end task or message delivery SLA.

Tradeoff: an attached/visible inbox may issue about six read-only inbox
observations per minute instead of the previous roughly two (four extra
requests per minute). The extra call fetches **only the selected inbox**
and is suppressed while a previous inbox read is pending;
it does not multiply full Agent/Conversation inventory traffic.

## Before/after test evidence (no build)

The new React Hook/Vitest fake-clock regression was first run against
unmodified main's 30s polling behavior. At a virtual 10s elapsed, the
test correctly failed with **one inbox read**, not the required second
read. The exact same test passed after the candidate was implemented:
a new delivery was present on the second read at virtual 10s, while the
global Agent inventory count was unchanged.

Additional regressions cover no Endpoint, slow in-flight requests without
overlapping reads, hidden-page pause, immediate foreground refresh and
independent Endpoint lease renewal. The existing focused React suites
passed **17/17** tests. All tests are deterministic fake-clock observations;
there was **no real remote Agent message or network latency A/B**, and no
Cargo or frontend bundle build.

The canonical right next action for real-world validation is to measure
message-post timestamp to visible inbox presentation with the same backend,
authentication and foreground-tab conditions on both versions. Keep
read-only capability and stale Endpoint fences unchanged.

## Maintainer review: shared refresh boundary

The initial single-flight check covered only the new 10s timer. A deterministic
review regression started a slow inbox read at 10s, then advanced through the
existing 30s inventory tick. The submitted implementation aborted that read
and started a third request instead of retaining the second. The same test
passes after separating scope cleanup from ordinary refresh revisions.

All inbox refresh sources now share one pending read and at most one queued
refresh. Inventory and explicit refreshes do not abort that observation;
Endpoint/client/authority changes and disposal still abort it and reject late
responses. A queued refresh does not start after the page becomes hidden.
Additional tests cover queued-refresh coalescing, a late error from a replaced
Endpoint, disabled views, and hidden-to-visible resumption. The independent
Endpoint renewal cadence and business mutation/replay behavior are unchanged.
