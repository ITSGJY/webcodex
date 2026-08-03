# @yyjeqhc/webcodex

[English](#english) | [简体中文](#简体中文)

## English

WebCodex lets ChatGPT, Claude, and other MCP clients work on private repositories through a local Runner while source files and command execution stay on the machine that owns the code.

### Install and connect

Supported in v0.3.1: Linux x64 and macOS arm64. Node.js 18 or newer is required for the installer wrapper.

```bash
npm install -g @yyjeqhc/webcodex
cd /path/to/your/repository
webcodex connect https://sg4.yyjeqhc.cn
```

`connect` uses the current directory by default, generates a strong shared key, writes an owner-only profile, starts a detached background Runner, and waits until the hosted Server can see the Runner and project. Copy the generated key immediately into the MCP client; it is printed in full only when first created. The output also gives the profile, config path, and log path.

The Runner survives terminal closure but not a machine reboot. After reboot, rerun the same `connect` command or use:

```bash
webcodex agent start --profile <profile>
```

Advanced users can provide `--key-file`, `--key`, or `--project`. Keep shared keys and generated `agent.toml` files out of Git.

### Package layout and integrity

The npm package exposes one public command: `webcodex`. During installation it downloads one platform artifact containing `webcodex`, `webcodex-server`, and `webcodex-runner`, verifies the manifest SHA-256, validates that all three binaries share one version/build identity, and atomically replaces the prior `vendor/bin` set. A failed download, checksum, extraction, or validation leaves the previous complete installation intact.

`webcodex-server` and `webcodex-runner` are intentionally not npm `bin` entries. The public command discovers those package-local executables for `webcodex server run` and `webcodex agent run`.

Release operators build and package one platform at a time:

```bash
cargo build --release -p webcodex-cli --bin webcodex -p webcodex --bin webcodex-server -p webcodex-runner --bin webcodex-runner
bash scripts/package_release_artifact.sh
```

Do not publish npm until every artifact declared in `manifest.json` has been uploaded immutably and its exact SHA-256 has replaced the placeholder.

### Disclaimer

WebCodex is provided only for research and learning. It can read and modify files and execute commands inside configured project boundaries. Use version control and backups. The author is not responsible for filesystem damage, data loss, or other consequences arising from use of the software.

## 简体中文

WebCodex 让 ChatGPT、Claude 和其他 MCP client 通过本地 Runner 操作私有仓库；源码、文件修改和命令执行仍留在持有代码的机器上。

### 安装与接入

v0.3.1 支持 Linux x64 和 macOS arm64。npm installer wrapper 需要 Node.js 18 或更新版本。

```bash
npm install -g @yyjeqhc/webcodex
cd /path/to/your/repository
webcodex connect https://sg4.yyjeqhc.cn
```

`connect` 默认使用当前目录，自动生成强随机 shared key，写入 owner-only profile，启动 detached 后台 Runner，并等待托管 Server 确实能看到 Runner 与项目。生成的 key 只在首次创建时完整显示，请立即复制到 MCP client；输出也会给出 profile、配置路径和日志路径。

关闭终端不会停止 Runner，但机器重启会终止它。重启后重新执行同一条 `connect`，或运行：

```bash
webcodex agent start --profile <profile>
```

高级用户可以使用 `--key-file`、`--key` 或 `--project`。不要把 shared key 或生成的 `agent.toml` 提交进 Git。

### Package 与完整性

npm package 只暴露一个公共命令：`webcodex`。安装时会下载包含 `webcodex`、`webcodex-server` 和 `webcodex-runner` 的平台 artifact，校验 manifest SHA-256，确认三个二进制具有相同版本和 build identity，再原子替换旧的 `vendor/bin`。下载、checksum、解压或校验失败时，旧的完整安装保持不变。

`webcodex-server` 与 `webcodex-runner` 不作为 npm `bin` 暴露；公共 `webcodex` 命令会在执行 `webcodex server run` 或 `webcodex agent run` 时发现 package 内部的二进制。

只有 `manifest.json` 中声明的每个平台 artifact 都已经不可变上传，并写入实际 SHA-256 后，才能发布 npm。

### 免责声明

WebCodex 仅用于研究与学习。它能够在配置的项目边界内读取、修改文件并执行命令；请使用版本控制和备份。若因使用本软件造成文件系统损坏、数据丢失或其他后果，作者概不负责。

## Acknowledgements / 鸣谢

Thanks to the [LINUX DO](https://linux.do/) community for its welcoming space for technical discussion and support for open-source sharing.

感谢 [LINUX DO](https://linux.do/) 社区提供的交流氛围与开源推广支持。

## Development verification / 开发验证

```bash
npm --prefix npm/webcodex test
bash scripts/npm_package_smoke.sh
```

## License

Apache-2.0. See the repository `LICENSE` file.
