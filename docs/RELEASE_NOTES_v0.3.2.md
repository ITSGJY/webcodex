# WebCodex 0.3.2

[English](RELEASE_NOTES_v0.3.2.md) | [简体中文](RELEASE_NOTES_v0.3.2.zh-CN.md)

WebCodex 0.3.2 makes the product easier to install and operate while simplifying
the coding tool surface and tightening mixed-version Runner behavior.

## Highlights

- **Server-only Docker Compose deployment.** The repository now includes a
  hardened Dockerfile, Compose file, environment template, and bootstrap script
  for running the coordination Server without placing repositories or Runner
  toolchains in the container.
- **Safer non-root Runner services.** Runner lifecycle commands support explicit
  user and system service scopes. Non-root users can install a persistent
  `systemctl --user` service without `sudo`; system services require an explicit
  Runner account, and root execution requires an explicit opt-in.
- **Clearer credential guidance.** CLI diagnostics and documentation distinguish
  user/runtime credentials from Agent transport tokens without weakening the
  existing Server authorization boundary.
- **Capability-gated project registration.** A Server can resolve or register a
  project from an absolute Runner path only when the connected Runner advertises
  the required capability. New Servers reject older Runners before sending an
  unsupported internal request.
- **Smaller editing surface.** Retired single-purpose edit tools and compatibility
  branches have been removed. The supported write path is centered on whole-file
  writes, transactional text edits, and checked patches. Unknown or retired
  `file_*` requests now fail before provider or shell fallback.
- **Shorter public documentation.** The README now focuses on the product,
  installation, hosted connection, Docker self-hosting, and the everyday tasks
  an online AI assistant can perform on connected machines.

## Quick start

```bash
npm install -g @yyjeqhc/webcodex
cd /path/to/your/repository
webcodex connect https://sg4.yyjeqhc.cn
```

Add the MCP URL and generated key printed by `connect` to ChatGPT or Claude, then
ask it to inspect files, edit code, run tests, or work with Git.

## Docker self-hosting

The included image is intentionally server-only: it contains
`webcodex-server` and the administrative `webcodex` CLI, but not
`webcodex-runner`, repositories, or language toolchains.

```bash
git clone --branch v0.3.2 --depth 1 \
  https://github.com/yyjeqhc/webcodex.git
cd webcodex
./deploy/docker/bootstrap.sh https://webcodex.example.com
```

The current Compose path builds from the tagged source checkout. Publishing the
same server-only image to GHCR or Docker Hub is a separate release operation and
is not required for a valid source or binary release.

## Breaking changes

- The seven retired single-purpose edit tools are no longer exposed. Use
  transactional text edits, checked patches, or whole-file writes instead.
- MCP and GPT Actions clients that cache schemas must refresh them before using
  the 0.3.2 tool surface.
- Old Servers that send retired or unknown `file_*` request kinds to a 0.3.2
  Runner receive a deterministic unsupported-request failure instead of shell
  or provider fallback.
- Mixed Server and Runner versions are not recommended for this upgrade.

## Upgrade notes

1. Upgrade `webcodex`, `webcodex-server`, and `webcodex-runner` together from
   the same v0.3.2 tagged revision.
2. Restart the Server and each Runner, then verify that all binaries report
   `0.3.2`, the same clean build revision, and `dirty=false`.
3. Refresh cached MCP or GPT Actions schemas because the legacy edit tool
   surface has been removed.
4. Existing hosted profiles and managed credentials remain separate; an Agent
   token still cannot be used for project or runtime APIs.
5. Non-root Runner installations should prefer `--scope user`. Review existing
   system services before reinstalling them under the new scope model.

## Binary packaging

The planned binary artifacts are:

- `webcodex-v0.3.2-linux-x64.tar.gz`
- `webcodex-v0.3.2-linux-arm64.tar.gz`
- `webcodex-v0.3.2-darwin-arm64.tar.gz`

Each artifact must contain `webcodex`, `webcodex-server`, and
`webcodex-runner` built from the exact immutable `v0.3.2` tag. The npm package
must not be published until the exact uploaded bytes have been recorded in the
release manifest with their real SHA-256 checksums.

## Known limitations

- The npm package does not currently cover Windows, macOS x64, or other targets.
- The Docker container runs only the coordination Server; every repository
  machine still needs a Runner.
- A detached hosted Runner survives terminal closure but must be restarted after
  a machine reboot unless it is installed as an OS service.
- Connected clients can modify files and execute commands within configured
  boundaries. Use version control, recoverable backups, and appropriately
  scoped OS users.

## Validation before release

The final tagged candidate should pass formatting, workspace compilation and
tests, hosted-connect and Runner lifecycle E2E coverage, npm package smoke,
release-binary identity checks, Docker build/health smoke, Markdown local-link
validation, and clean-worktree review.

## Next steps

After upgrading, refresh the client schema, verify Server and Runner build
identities, and run one read-only project task before resuming write access.

## Acknowledgements

Thanks to the [LINUX DO](https://linux.do/) community for its welcoming space
for technical discussion and support for open-source sharing.
