# Messages 网关

Rust 桌面网关，原生转发 Messages、Chat Completions 和 Responses，支持 StepFun 与 OpenCode Zen。默认地址为 `http://127.0.0.1:8789`，界面提供上游配置、模型列表、Token 与缓存统计、最近 100 条请求及完整请求/响应差异。

## 下载与运行

从 [最新 Release](https://github.com/ukjent7/test/releases/latest) 下载：

- [Windows x64](https://github.com/ukjent7/test/releases/latest/download/messages-gateway.exe)：放到可写目录，双击运行；需要系统 WebView2。
- [Linux x64](https://github.com/ukjent7/test/releases/latest/download/messages-gateway-linux-x64)：安装 `libgtk-3-0`、`libwebkit2gtk-4.1-0`，执行 `chmod +x messages-gateway-linux-x64` 后运行。
- [SHA256SUMS](https://github.com/ukjent7/test/releases/latest/download/SHA256SUMS)：用于核验下载文件。

关闭窗口即退出网关；停止转发只阻止新请求。添加 `--headless` 可无窗口运行，并通过本地地址访问管理页面。更新时退出程序并替换可执行文件，保留同目录的数据。

## 客户端接入

在界面分别配置两边的上游地址与密钥，拉取模型列表后选择模型。默认上游为：

- StepFun：`https://api.stepfun.ai/step_plan/v1`，模型使用 `stepfun/` 前缀；无前缀模型也走 StepFun。
- OpenCode Zen：`https://opencode.ai/zen/v1`，模型使用 `opencode/` 前缀。

网关去掉模型前缀后转发，不自动转换协议或切换上游。接口支持 `/v1/messages`、`/v1/chat/completions`、`/v1/responses` 及不带 `/v1` 的路径；`GET /v1/models` 合并两边模型列表，`GET /health` 用于检查运行状态。

Grok Build 接入示例，编辑 `%USERPROFILE%\.grok\config.toml`：

```toml
[model.step]
model = "stepfun/step-5-preview"
base_url = "http://127.0.0.1:8789/v1"
api_key = ""
api_backend = "messages"

[model.zen]
model = "opencode/mimo-v2.5-free"
base_url = "http://127.0.0.1:8789/v1"
api_key = ""
api_backend = "chat_completions"
```

保留其它模型设置，保存后重启 Grok Build。配置的上游密钥优先于客户端密钥；未配置时透传客户端密钥。Zen 匿名免费模型可使用 `public`，模型可用性以实际拉取结果为准。网关不会修改 Grok 配置或读取 `auth.json`。

## 配置与数据

设置保存后立即用于新请求。界面密钥框留空会保留已有密钥。默认数据跟随可执行文件目录，不随启动工作目录变化，也不写入 AppData/XDG；目录不可写时显示失败。

“网络代理”默认使用系统代理（Windows Internet Options 或 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 环境变量），也可选择直连或自定义 HTTP(S)、SOCKS5/SOCKS5H 代理；无协议的 `主机:端口` 按 HTTP 处理。StepFun 和 OpenCode Zen 可分别关闭代理，开关同时作用于模型列表和三个转发协议。本地回环地址始终直连；保存设置后刷新代理配置，重启后保留选择。

- `settings.json`：设置及上游密钥。
- `requests.sqlite3`：最近 100 条完整请求和持续累计的用量，重启后保留；包含完整用户正文、思考和工具参数，鉴权头与 Cookie 只保存指纹。
- `webview/`：桌面浏览器缓存、Cookie 和主题数据。

“统计”页按今天、近 7 天、近 30 天、全部展示总览、供应商和供应商模型的输入/输出 Token、缓存读取/写入与命中率。时间范围按本地自然日计算，命中率按已报告缓存的请求输入 Token 加权；缺失与部分报告明确标注。升级会纳入尚存的最近 100 条历史，旧记录没有输出 Token 时保持未报告。

可通过环境变量覆盖配置：

| 变量 | 用途 |
| --- | --- |
| `GATEWAY_LISTEN` | 监听地址，默认 `127.0.0.1:8789` |
| `GATEWAY_UPSTREAM_BASE_URL` / `GATEWAY_OPENCODE_BASE_URL` | StepFun / Zen 上游基地址 |
| `GATEWAY_STEPFUN_API_KEY` / `GATEWAY_OPENCODE_API_KEY` | StepFun / Zen 密钥 |
| `GATEWAY_CONFIG` | 设置文件路径 |
| `GATEWAY_DB` | 请求数据库路径 |
| `WEBVIEW2_USER_DATA_FOLDER` | 桌面 WebView 数据目录 |

## 兼容处理

- Messages 补齐 thinking 块缺失或 null 的 `thinking` / `signature`，不伪造签名；历史无签名思考转为文本，有签名内容原样保留。
- Zen 使用必要的请求头与稳定会话标识；Chat/Responses 仅在收到 `403 FreeTierError` 时补充免费层所需形态并最多重发一次，非流式客户端仍收到 JSON。
- 正常响应保持原生转发，SSE 及时发送；活动记录区分缓存用量为零与上游未报告，可查看完整正文和字节差异。

## 开发与验证

使用 Rust stable、edition 2024，提交依赖锁文件。遵循 [AGENTS.md](AGENTS.md)；本机不安装 Rust、不编译，构建和验证交给 [GitHub Actions](.github/workflows/ci.yml)。

后端按职责组织：`main` 启动程序，`app` 管理状态与本地界面接口，`forwarding` 转发 HTTP，`protocol` 处理 Messages/SSE 兼容，`settings` 读写配置与创建网络客户端；`app::trace` 与 `history` 记录并持久化请求，`zen` 封装 Zen 兼容处理。

前端使用原生 ES modules，随二进制打包，无需 Node 构建：`app.js` 管理导航和状态同步，`gateway` / `settings` 管理接入与设置，`activity` / `details` 展示历史与完整交换，`usage` 展示累计用量。方向键、Home / End 可切换页面；模型与协议选择会更新可复制的客户端配置。

CI 使用 `--locked`，执行格式检查、Clippy、Windows/Linux 构建及 HTTP、历史记录、GUI E2E；测试使用本地夹具，不需要个人 API key。Actions 的 `e2e-*` 产物保留请求/响应、截图、trace、报告和 SHA-256 清单，失败时也上传已有证据。

`main` 的全部检查通过后，工作流自动创建 `build-<运行序号>` tag 和 Release，附带两个平台的已验证二进制、SHA256SUMS 与 CI 链接。
