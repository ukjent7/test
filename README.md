# Messages 网关

Rust 编写的本地 HTTP 网关，初版仅支持 `POST /v1/messages` 和 `POST /messages`。默认监听 `127.0.0.1:8789`，上游为 `https://api.stepfun.ai/step_plan/v1`。

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

修改 `%USERPROFILE%\.grok\config.toml` 中已有的 StepFun 模型，只替换 `base_url`：

```toml
[model.step]
model = "step-5-preview"
base_url = "http://127.0.0.1:8789/v1"
api_key = "原有 StepFun key"
api_backend = "messages"
```

保留该模型的其它设置。网关透传 Grok 的 `Authorization` 或 `x-api-key`，无需另存密钥。启动网关后重新启动 Grok Build 并发送消息。网关不修改本机 Grok 配置，也不读取 auth.json。

更换上游或端口：

```powershell
$env:GATEWAY_UPSTREAM_BASE_URL = "https://api.stepfun.ai/step_plan/v1"
$env:GATEWAY_LISTEN = "127.0.0.1:8789"
.\messages-gateway.exe
```

上游配置填写基地址，网关追加 `/messages`；请求查询参数原样转发。`GET /health` 返回 `ok`。其它推理协议返回 404。HTTP 错误及重定向原样交回客户端，不自动重试或跟随重定向。

## GitHub Actions 验证

本机不安装 Rust、不编译、不执行 E2E。CI 在 Linux 和 Windows 上编译、运行 Clippy，并启动真实网关与 HTTP 夹具上游进行 E2E。测试先验证日志原始事件被 Grok 实际生产 wire types 拒绝，再验证所有修补后的正常事件能被同一类型解析。wire types 从固定 Git revision 下载并检查 SHA-256，不改写为简化测试类型。

场景详见 [tests/FAILURE_MODES.md](tests/FAILURE_MODES.md)。成功运行提供两类 Actions 产物：

- `messages-gateway-*`：对应操作系统的可执行文件。
- `e2e-*`：上下游请求、原始响应、实际 Grok 解析结果、流首事件记录、汇总 `report.json`、源代码版本、依赖锁文件和 `sha256.json`。失败时也保存已有证据。

在有 Rust 的环境中重复 CI：

```bash
cargo build --release
python tests/fetch_grok_wire.py
cargo build --release --manifest-path tests/grok-wire/Cargo.toml
python tests/e2e.py --gateway target/release/messages-gateway --checker tests/grok-wire/target/release/grok-wire-check
```

Windows 可执行文件名追加 `.exe`。此 E2E 使用日志夹具和本地 HTTP 上游，不需要真实 API key；真实 StepFun 会话仍需使用你的模型 key 验证。
