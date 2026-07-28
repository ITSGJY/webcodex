# Console 前端开发

WebCodex 的正式 `/console` 资源仍由 `frontend/dist/` 编译进 Rust
二进制。开发资源模式只改变三个固定资源的来源：

- `/console` → `console.html`
- `/console/app.js` → `app.js`
- `/console/styles.css` → `styles.css`

它不启动独立前端服务器，不改变 API origin，也不提供任意目录静态文件服务。

## 启动开发构建

在 WebCodex 仓库中运行：

```bash
npm --prefix frontend install
npm --prefix frontend run dev
```

watcher 会立即完整构建一次，并将临时产物原子写入
`frontend/.dev-dist/`。此目录已被 Git 忽略，不应提交。它会监听：

- `frontend/src/app.ts`
- `frontend/src/review_state.ts`
- `frontend/src/styles.css`
- `frontend/src/console.html`

## 启动本地项目

在另一个终端中，对已经执行过 `webcodex setup` 的项目显式指定绝对路径：

```bash
webcodex agent start \
  --console-assets-dir /absolute/path/to/webcodex/frontend/.dev-dist
```

启动输出会显示实际 Console URL，例如：

```text
Console: http://127.0.0.1:32529/console
Console assets: local development
Assets directory: /absolute/path/to/webcodex/frontend/.dev-dist
```

打开该 `/console` URL。修改前端源码后等待 watcher 构建完成，再普通刷新浏览器
即可；不需要重新编译 Rust。第一版不自动刷新浏览器。

修改 Rust 或 API 后仍需重新编译并重启 WebCodex。

## 安全和缓存边界

- 开发资源模式默认关闭，每次 `agent start` 都必须显式传参。
- 开发资源模式只允许 loopback 绑定，不能用于 LAN 或公网部署。
- 启动时会 canonicalize 目录，并检查三个文件均存在、是可读普通文件且不是
  符号链接。
- 运行期间文件缺失或读取失败会返回 HTTP 500，不会回退到嵌入资源。
- 开发响应带 `Cache-Control: no-store`、`Pragma: no-cache` 和
  `X-WebCodex-Console-Assets: filesystem`。
- 绝对资源目录只显示在本机启动终端，不会写入浏览器 API。

高级用户直接运行 `webcodex serve` 时，也可以设置私有环境变量，但必须同时
使用 loopback 地址：

```bash
WEBCODEX_ADDR=127.0.0.1:8080 \
WEBCODEX_CONSOLE_ASSETS_DIR=/absolute/path/to/webcodex/frontend/.dev-dist \
webcodex serve
```

首选入口仍是 `webcodex agent start --console-assets-dir ...`。

## 正式产物

准备提交或发布前重新生成并检查正式嵌入产物：

```bash
npm --prefix frontend run build
npm --prefix frontend run check:dist
npm --prefix frontend run typecheck
npm --prefix frontend test
node --check frontend/dist/app.js
```

`frontend/dist/` 是应提交的正式产物；`frontend/.dev-dist/` 只用于本地开发。
