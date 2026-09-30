# Messages 网关

Rust 编写的桌面网关，支持 Messages、Chat Completions、Responses 的原生转发和模型列表拉取。默认监听 `127.0.0.1:8789`，两个上游分别为 StepFun `https://api.stepfun.ai/step_plan/v1` 和 OpenCode Zen `https://opencode.ai/zen/v1`，根据模型前缀路由。

## 桌面界面

双击 `messages-gateway.exe` 打开原生窗口并启动网关。系统 WebView 显示随程序内嵌的 HTML/CSS/JS，无须安装 Node 或前端资源。关闭窗口即退出程序和网关。

- 查看实时运行状态，开启或停止新请求转发；停止不打断已经开始的请求。
- 分别配置 StepFun 和 OpenCode Zen 地址与密钥；保存后立即用于新请求，并供下次启动使用。
- 拉取两个上游的模型列表，选择并复制带路由前缀的模型 ID。
- 复制 Grok Build 接入地址或配置片段。
- 查看本次运行请求数、修补数量、错误及最近 30 条请求。
- 在活动记录中点击「查看差异」，分别查看请求和响应被修改的内容，红色表示修改前，绿色表示修改后；支持复制差异。
- 切换并记住深浅主题。

活动记录只存在于内存中，包含模型、HTTP 状态、耗时、修补数量和被修改的区块。差异可能包含无签名思考文本，不记录请求头、API key 或未修改的正文；最多保留最近 30 条，退出后清空。每秒状态轮询只传输摘要，点击记录时才读取差异。

请求差异展示无签名 thinking 转成 text、空思考块或空助手消息的删除；数组删除后同时展示原始和转发位置。响应差异展示补齐的 thinking/signature，区分「字段不存在」、`null` 和 `""`；SSE 标记帧编号、事件类型与 content block index。已有签名、工具调用等未修改内容不会被误报。此视图比较 JSON 内容，不展示空白、键顺序或 HTTP 传输头的变化。

设置保存在系统用户配置目录的 `MessagesGateway/settings.json`，Windows 为 `%APPDATA%\MessagesGateway\settings.json`，包含两边地址和配置的密钥；旧版设置自动补上默认 Zen 地址与空密钥。`GATEWAY_CONFIG` 可指定文件；`GATEWAY_UPSTREAM_BASE_URL`、`GATEWAY_OPENCODE_BASE_URL`、`GATEWAY_STEPFUN_API_KEY`、`GATEWAY_OPENCODE_API_KEY` 在启动时覆盖对应设置。设置写入成功后才更新实际转发配置。GUI 管理接口只接受本机与同源请求，状态接口只返回密钥是否已配置，编辑框留空保留已保存密钥；管理 API 显式传空字符串可清除对应密钥。

Windows 使用系统 WebView2。Linux 原生窗口使用 GTK3 与 WebKitGTK 4.1，需安装 `libgtk-3-0` 和 `libwebkit2gtk-4.1-0`。命令行模式用 `messages-gateway --headless`，此时也可访问本地地址打开同一管理页面。

## Rust 与依赖

项目使用 edition 2024、对应的格式化规则和默认 resolver 3。`rust-toolchain.toml` 选择最新 `stable`，当前核查为 [Rust 1.98.1](https://github.com/rust-lang/rust/releases/tag/1.98.1)。直接依赖已更新为当前最新稳定版本，锁文件固定 CI 已验证的完整依赖图。

迁移参考本地 `edition-guide/src/rust-2024/`，采用 let-else、2024 格式化及普通引用模式，不在多线程运行期间修改进程环境变量。reqwest 0.13 使用新的 `rustls` 功能名。GUI 设置直接更新共享状态。

## 故障与修复

`sampling.jsonl` 在 `2026-09-30T20:29:49.5321928Z` 记录 StepFun 返回：

```json
{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}
```

Grok Build 的 `ContentBlock::Thinking` 将 `signature` 定义为必填 `String`，所以该事件触发 `missing field signature`。pi 在接收时使用 `thinking ?? ""`、`signature ?? ""`，后续签名由 `signature_delta` 提供。

网关只为 thinking 块补齐缺失或 null 的 `thinking` / `signature`，流式和非流式均适用；不生成虚假签名。已有签名、签名增量、redacted_thinking、正文、工具调用、用量及停止原因原样保留。SSE 每收到一个完整事件便转发，不等待完整回答。无须关闭 thinking。

Grok 会在下一轮把无签名思考内容也回传为 thinking；网关采用 pi 默认策略将这些历史块转为 text，保留内容和 cache_control；跳过空的无签名思考块及因此为空的 assistant 消息。有真实签名的历史块原样发送。

参考本地源码版本：

- Grok Build `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`，`crates/codegen/xai-grok-sampling-types/src/messages.rs` 与 `src/conversation/messages.rs`。
- pi `955cc6665ee3986c6a033db52200779310d10dfd`，`packages/ai/src/api/anthropic-messages.ts` 的接收与历史消息转换逻辑。
- [Messages 流式协议](https://platform.claude.com/docs/en/build-with-claude/streaming)定义 thinking_delta 和独立的 signature_delta。

## Windows 使用

从 GitHub Actions 的成功运行下载 `messages-gateway-windows-latest` 并解压。运行：

```powershell
.\messages-gateway.exe
```

在界面配置两边密钥后，修改 `%USERPROFILE%\.grok\config.toml` 中的模型配置：

```toml
[model.step]
model = "stepfun/step-5-preview"
base_url = "http://127.0.0.1:8789/v1"
api_key = ""
api_backend = "messages"
```

Zen 示例：

```toml
[model.zen]
model = "opencode/mimo-v2.5-free"
base_url = "http://127.0.0.1:8789/v1"
api_key = ""
api_backend = "chat_completions"
```

保留模型的其它设置。配置中的上游密钥优先于客户端传来的密钥，客户端无需提供网关密钥；上游密钥未配置时保留旧版的客户端密钥透传。没有前缀的模型仍走 StepFun，旧接入可以继续使用。启动网关后重新启动 Grok Build。网关不修改本机 Grok 配置，也不读取 auth.json。

更换上游或端口：

```powershell
$env:GATEWAY_UPSTREAM_BASE_URL = "https://api.stepfun.ai/step_plan/v1"
$env:GATEWAY_LISTEN = "127.0.0.1:8789"
.\messages-gateway.exe
```

上游配置填写基地址，网关根据客户端接口追加 `/messages`、`/chat/completions` 或 `/responses`；三个接口均接受带 `/v1` 和不带 `/v1` 的路径，请求查询参数原样转发。`GET /v1/models`（或 `/models`）并行拉取两边 `/models`，保留每项元数据并为 ID 加上 `stepfun/` 或 `opencode/`。某边失败时返回另一边的列表和 `upstream_errors`，两边都失败时返回 502；不缓存模型目录。`GET /health` 返回 `ok`，其它推理协议返回 404。HTTP 错误及重定向原样交回客户端，不跟随重定向；只有下述 Zen 免费层形态错误会修正后重发一次。

## OpenCode Zen 上游

在「编辑上游」中配置 Zen 地址和密钥，匿名免费模型可填 `public`。客户端模型填写 `opencode/<上游模型 ID>`，例如 `opencode/mimo-v2.5-free`；模型前缀在发往上游前删除，活动记录与请求 diff 保留路由证据。`stepfun/step-5-preview` 走 StepFun。没有自动故障切换，也不自动转换协议：据[官方 Zen 模型列表](https://opencode.ai/docs/en/zen/)，MiMo-V2.5 Free 使用 Chat Completions，客户端应选择该协议；Claude 模型使用 Messages，Responses 模型使用 Responses。正常 Chat/Responses 正文和响应原样透传，已有 thinking 修补仅作用于 Messages。

仅当实际转发目标主机为 `opencode.ai`、路径为 `/zen/v1/messages`、`/zen/v1/chat/completions`、`/zen/v1/responses` 或 `/zen/v1/models` 时进行必要伪装。参照本地 `opencode2api`（`79a208a4679106c4643edb1efa1a5f79011e0a6e`）的 `internal/httpx/client.go`、`internal/identity/request.go` 与 `internal/gateway/upstream.go`，发送 `User-Agent: opencode/1.18.31` 和 `x-opencode-session`。会话格式采用 `ses_` + 12 小写十六进制字符 + 14 Base62 字符；合法 OpenCode 会话原样保留，其它会话标识在当前构建中确定性映射。优先使用 `x-opencode-session`、`x-session-affinity`、`x-session-id`、`conversation-id`、正文 `metadata.session_id` / `conversation_id`；没有标识时使用第一条用户消息或 Responses input，后续历史增长不改变会话。映射包含实际上游凭证以区分账户，结果仅用于路由关联，不是鉴权凭证。

Zen 请求头按白名单重建：Messages 使用 `x-api-key`，保留 `anthropic-version` / `anthropic-beta`，版本缺失时补 `2023-06-01`；Chat/Responses/模型列表使用 Bearer 鉴权，不发送 Anthropic 头。设置 JSON Content-Type、JSON/SSE Accept 和 `Accept-Encoding: identity`，由 HTTP 客户端重算 Host 与 Content-Length。原客户端 User-Agent、Cookie、Forwarded、SDK 和追踪头全部清除。其它上游继续原有透传规则，配置密钥覆盖客户端凭证。

必要性分析：参考项目的 UA 与会话形态可保留；项目/请求 ID、额外会话关联头、client 标记和固定 Anthropic beta 没有纳入实现。[会话 ID 源码](https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/id/id.ts)定义格式，[官方 Zen handler](https://github.com/anomalyco/opencode/blob/dev/packages/console/app/src/routes/zen/util/handler.ts)读取会话信息用于关联和路由。2026-09-30 的 Actions 线上探测发现，MiMo 仅有头伪装时返回 403 FreeTierError，单独加 stream 或五个工具也被拒绝，二者同时存在则返回 200；工具选择设为 none 仍返回 200。证据：[组合对比](https://github.com/ukjent7/test/actions/runs/36790864322)、[逐项对比](https://github.com/ukjent7/test/actions/runs/36791039662)、[禁用占位工具调用](https://github.com/ukjent7/test/actions/runs/36791436498)。这些结果比旧接入报告更贴近当前部署。

因此，只对 Zen Chat/Responses 的 `403 + error.type == FreeTierError` 补充 `stream: true` 和缺少的 `bash/edit/glob/grep/read` 工具，沿用原会话、密钥、地址重发一次；已满足形态时不重发，不硬编码免费模型列表，不改变正常付费请求。已有工具、参数和工具选择保留；原本没有工具且没有指定工具选择时设置 `tool_choice: none`，占位定义也标明不应调用。若客户端要求非流式，Chat 合并文字、reasoning、工具增量、多 choice 和 usage，Responses 取终止事件的完整 response，返回 JSON；缺少终止事件、坏 JSON 或网络截断返回 502。原本要求流式时继续即时透传。必要的正文修改记录在请求 diff；stream/tools 外的原有正文保持不变。

`tests/fixtures/zen-chat.sse` 保存上述真实 MiMo 流用于确定性回归。线上探测使用 public 凭证与虚构测试消息，没有使用配置中的个人密钥；探测的请求、响应、报告和 SHA-256 清单由 Actions 归档。

[最终线上接入验证](https://github.com/ukjent7/test/actions/runs/36792610882)：真实网关使用配置中的 public 密钥，客户端不带密钥，以 `opencode/mimo-v2.5-free` 发送普通非流式请求；网关修正免费层形态并返回 200 Chat JSON，内容为 OK。临时线上探测工作流已移除，常规 CI 继续使用本机夹具以保证回归可重复；历史探测可从对应 Actions 运行重放。

CI 的 HTTP E2E 通过本机代理捕获以 `opencode.ai` 为目标的真实网关请求，验证模型列表、按前缀路由、配置密钥覆盖、无客户端密钥、头清洗、会话稳定性、三种协议的 JSON/SSE、免费层形态修正与流合并、相近域名与路径不触发伪装，并保留请求/响应及 SHA-256 清单；GUI E2E 保存两个上游的设置、模型列表、密钥隐藏和重启持久化证据。

## GitHub Actions 验证

参考 [Wry](https://github.com/tauri-apps/wry/blob/dev/.github/workflows/clippy-fmt.yml)、[Tauri](https://github.com/tauri-apps/tauri/blob/dev/.github/workflows/test-api-e2e.yml) 和 [uv](https://github.com/astral-sh/uv/blob/main/.github/workflows/ci.yml) 的编排。格式检查与 Windows/Linux 构建并行；Clippy 使用 release 配置，Grok 校验器复用同一个 target 目录，避免单独重建依赖。Rust、pip 和 Linux 浏览器均有缓存，E2E 失败也保留 Rust 缓存。同分支新提交取消旧任务，push 只检查 main，其他分支通过 PR 检查，避免一份变更触发两组 CI。

CI 不修改源文件、不执行 cargo update，始终用已提交的锁文件构建；Dependabot 每周分组更新 Rust、Python 和 Actions 依赖，通过同一套 E2E 验证后合入。`All checks passed` 要求格式检查及两个平台的完整构建/E2E 均成功，单个平台中某一步通过不代表整组完成。

本机不安装 Rust、不编译、不执行 E2E。CI 在 Linux 和 Windows 上编译、运行 Clippy，并启动真实网关与 HTTP 夹具上游进行 E2E。测试先验证日志原始事件被 Grok 实际生产 wire types 拒绝，再验证所有修补后的正常事件能被同一类型解析。wire types 从固定 Git revision 下载并检查 SHA-256，不改写为简化测试类型。

场景详见 [tests/FAILURE_MODES.md](tests/FAILURE_MODES.md)。成功运行提供两类 Actions 产物：

- `messages-gateway-*`：对应操作系统的可执行文件。
- `e2e-*`：协议请求/响应、Grok 解析结果，以及 GUI 与 diff 的浅色/深色/窄窗口截图、JSON/SSE 差异快照、交互 trace、状态快照、汇总 `report.json`、工具链版本、依赖锁文件和 `sha256.json`。Windows GUI 测试操作真正的桌面 WebView2；Linux 测试同时验证原生窗口启动与相同页面的控件。失败时也保存已有证据。

在有 Rust 的环境中重复 CI：

```bash
cargo build --locked --release
python tests/fetch_grok_wire.py
cargo build --locked --release --manifest-path tests/grok-wire/Cargo.toml --target-dir target
python tests/e2e.py --gateway target/release/messages-gateway --checker target/release/grok-wire-check
python -m pip install -r tests/requirements.txt
# Windows：操作原生 WebView2
python tests/gui_e2e.py --gateway target/release/messages-gateway.exe
# Linux：安装 Chromium 后在 Xvfb 中验证原生窗口与页面
python -m playwright install --with-deps chromium
dbus-run-session -- xvfb-run -a python tests/gui_e2e.py --gateway target/release/messages-gateway
```

Windows 可执行文件名追加 `.exe`。此 E2E 使用日志夹具和本地 HTTP 上游，不需要真实 API key；真实 StepFun 会话仍需使用你的模型 key 验证。
