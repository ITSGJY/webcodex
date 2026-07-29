# @yyjeqhc/webcodex

Thin npm installer and public command wrapper for WebCodex.

```bash
npm install -g @yyjeqhc/webcodex
webcodex --help
```

The npm package exposes one command only: `webcodex`. The wrapper launches the package-local native `webcodex` executable with inherited standard streams, unchanged arguments, exit status, and terminal signals.

During installation, `install.js` installs one atomic runtime set into `vendor/bin`:

- `webcodex`
- `webcodex-server`
- `webcodex-runner`

`webcodex-server` and `webcodex-runner` are intentionally not npm `bin` entries. The public `webcodex` command finds those internal executables beside itself when `webcodex server run` or `webcodex agent run` is used. No `webcodex-cli` executable, wrapper, or symlink is installed.

## Artifact and integrity model

The installer recognizes `linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64`, and `win32-x64`. The artifacts actually declared in a manifest are that release's platform coverage; recognized platforms are not implicitly required to be published. Each declared artifact contains all three executables from one build. The installer downloads to a temporary file, verifies SHA-256, extracts to a staging directory, checks all three regular files, sets Unix execute permissions, runs bounded `--version` checks, verifies one shared build identity, and atomically replaces the prior `vendor/bin` directory. A failed download, checksum, extraction, or validation leaves the previous complete installation intact.

Current v0.3.0 release coverage is `linux-x64`. Other recognized platform keys fail clearly as unavailable unless the manifest declares a matching artifact. Release operators build all three binaries together and create an artifact with:

```bash
cargo build --release -p webcodex-cli --bin webcodex -p webcodex --bin webcodex-server -p webcodex-runner --bin webcodex-runner
bash scripts/package_release_artifact.sh
```

Do not publish npm until the checksum in `manifest.json` matches the immutable uploaded artifact.

## Development switches

- `WEBCODEX_SKIP_DOWNLOAD=1` skips installation.
- `WEBCODEX_BINARY_DIR=/path/to/bin` atomically copies a local three-binary build.
- `WEBCODEX_MANIFEST=/path/to/manifest.json` or a file/HTTP URL selects another manifest.

## Local verification

```bash
npm --prefix npm/webcodex test
bash scripts/npm_package_smoke.sh
```

The smoke builds all three binaries, inspects `npm pack --dry-run`, creates and unpacks a local tarball, installs it into a temporary npm prefix, and verifies the public wrapper plus same-directory Server and Runner discovery. It does not publish.

## License

Apache-2.0. See the repository `LICENSE` file.
