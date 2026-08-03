# WebCodex 0.3.1

[English](RELEASE_NOTES_v0.3.1.md) | [简体中文](RELEASE_NOTES_v0.3.1.zh-CN.md)

WebCodex 0.3.1 focuses on making the official hosted path practical for a new user while tightening Runner transport and job-recovery behavior.

## Highlights

- **One-command hosted onboarding.** After installing the npm package, a user can enter a repository and run `webcodex connect https://sg4.yyjeqhc.cn`. The CLI generates a strong shared key, writes a project-bounded profile, starts one detached Runner, and verifies that the hosted Server can see both the Runner and project.
- **Safer shared-key operation.** Hosted Runner registrations are bounded, generated keys are disclosed only once, logs rotate with bounded retention, and shared-key expiry no longer rewrites already-lost jobs into a misleading terminal state.
- **Unified transport supervision.** WebSocket and QUIC use one reconnect, shutdown, fallback, and error-classification lifecycle. Auto mode can fall back to WebSocket, while strict QUIC treats certificate failures as fatal and transient network failures as retryable.
- **macOS persistent-shell compatibility.** Darwin now creates the shell control pipe with `pipe` and immediately applies `FD_CLOEXEC`, while Linux/Android retain atomic `pipe2(O_CLOEXEC)` behavior.
- **More reliable job recovery.** Reconciliation preserves job identity and log cursors across Server restarts, distinguishes recoverable and legacy Runner disconnects, and keeps hidden handoff/cleanup jobs outside public history retention.
- **Lower-friction model workflows.** Startup briefs, continuation feedback, bounded batch file reads/searches, asynchronous validation jobs, SSH-backed Session contexts, and managed temporary projects reduce round trips without widening project boundaries.
- **Bilingual release documentation.** The repository and npm package now carry English and Simplified Chinese onboarding, platform, recovery, disclaimer, and LINUX DO acknowledgement sections.

## Hosted Quick Start

Release artifacts are available for Linux x64, Linux arm64, and macOS arm64:

```bash
npm install -g @yyjeqhc/webcodex
cd /path/to/your/repository
webcodex connect https://sg4.yyjeqhc.cn
```

Copy the generated MCP key immediately. It is stored in the owner-only profile but is not printed again by status or log commands. A detached hosted Runner survives terminal closure but not a machine reboot; after reboot, rerun `connect` or use `webcodex agent start --profile <profile>`.

## Upgrade Notes

1. Upgrade `webcodex`, `webcodex-server`, and `webcodex-runner` together from the same v0.3.1 artifact/build revision.
2. Restart the Server and each Runner, then verify that all binaries report `0.3.1`, the same commit, and `dirty=false`.
3. Refresh cached MCP or GPT Actions schemas when a client retains an older tool list.
4. Existing managed credentials and hosted profiles remain separate. A `wc_*` managed credential never falls back to shared-key authentication.

No intentional public CLI command or canonical MCP operation is removed in this patch release.

## Packaging

The npm package is a thin installer/wrapper. The v0.3.1 manifest declares:

- `webcodex-v0.3.1-linux-x64.tar.gz`
- `webcodex-v0.3.1-linux-arm64.tar.gz`
- `webcodex-v0.3.1-darwin-arm64.tar.gz`

Each artifact contains `webcodex`, `webcodex-server`, and `webcodex-runner` from one clean tagged revision. The release-preparation tag intentionally keeps checksum placeholders. npm must not be published until all three immutable artifacts are uploaded and the exact SHA-256 values are committed in a reported post-tag manifest commit without moving `v0.3.1`.

## Known Limitations

- A hosted shared key is a capability credential. Anyone who possesses it can use the permissions of the associated project-bounded Runner profile.
- The detached Runner is not an OS startup service and must be restarted after a machine reboot.
- macOS x64 and Windows artifacts are not part of the v0.3.1 npm coverage.
- The browser console is a review and operations surface, not a full IDE.
- Production safety still depends on HTTPS, scoped credentials, OS-user isolation, backups, and operator review.

## Disclaimer

WebCodex is provided only for research and learning. It can read and modify files and execute commands inside configured project boundaries. Use it only with version control and recoverable backups. The author is not responsible for filesystem damage, data loss, or other consequences arising from use of the software.

## Validation

The release candidate is required to pass formatting, workspace compilation/tests, hosted-connect and job-recovery real-process E2E coverage, npm installer/package smoke, release-binary identity checks, Markdown local-link validation, and clean-worktree/hygiene review before the tag is created.

## Acknowledgements

Thanks to the [LINUX DO](https://linux.do/) community for its welcoming space for technical discussion and support for open-source sharing.
