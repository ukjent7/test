# 实现前确定的失败场景

端到端验证经过真实 HTTP 上游、Rust 网关进程和客户端，禁止用适配函数的单元测试替代。

1. 日志中的 `thinking` 起始块缺少 `signature`，Grok 的必填字段反序列化失败。
2. 起始签名为 null，或 thinking 文本未初始化；应采用 pi 的空字符串默认值。
3. 已有签名、分段 signature_delta、多 thinking 块、redacted_thinking 被修改或丢失。
4. SSE 跨 HTTP 分片，分片落在中文 UTF-8 字符、JSON 或 CRLF 分隔符中；事件不能损坏。
5. SSE 包含多行 data、注释、id、retry、ping；修补应保留这些字段与事件顺序。
6. 非流式响应或 message_start 内已有 thinking 内容也缺少 signature。
7. 无签名思考内容被 Grok 回传为 thinking 块；按 pi 默认策略转为 text，已有签名和 redacted_thinking 原样回传。
8. 空的无签名 thinking 需移除，但不能移除同消息中的工具调用、正文；仅含空 thinking 的 assistant 消息需跳过。
9. 工具 JSON 增量、tool_result、用量和停止原因在透传中改变。
10. Authorization、x-api-key、anthropic-version、anthropic-beta、查询参数、上游路径未正确转发。
11. 修补后沿用旧 Content-Length；Connection 指定的逐跳头被错误转发。
12. 上游 401/429/500 的状态码、错误体和 Retry-After 丢失；上游重定向被自动跟随。
13. 成功响应中不合法的 JSON 被伪造成成功；SSE 中不合法的 data 被悄悄丢弃。
14. 上游流中途断开被伪造成正常结束；网关等待完整回答才发首个事件。
15. `/responses`、`/chat/completions` 意外启用，或错误消息被当成正常 message 修补。

CI 产物：脱敏的故障日志夹具、每个场景的上下游请求/响应、首事件及时性记录、Grok 实际生产 wire types 反序列化结果、汇总报告及 SHA-256 清单。失败时也上传已有证据。

## 第二版：先确定 GUI 与迁移失败场景

1. 双击程序未创建真正的桌面 WebView 窗口，或 GUI 和转发服务启动顺序导致空白页。
2. 界面显示的运行状态、地址、请求数、修补数与真实网关不一致。
3. 停止后仍发起上游请求；重新启动后不能继续修补 Messages 响应。
4. 修改上游后未立即生效；重启丢失保存的设置；无效地址覆盖可用设置。
5. 保存失败却显示成功；环境变量覆盖规则不明确。
6. 请求记录泄露 API key 或用户消息，或不记录上游错误、取消和修补数量。
7. 复制接入地址、设置对话框、键盘 Escape 或深浅主题在实际界面中失效。
8. 窄窗口内容溢出、深色模式不可读，或轮询重绘打断用户输入。
9. GUI 管理接口允许远端请求或跨站点修改上游地址。
10. edition 2024 的作用域、导入排序、resolver 或 reqwest 0.13 TLS 功能变化使原有 16 场景回归。
11. GUI 启动时端口被占用，用户只看到程序静默退出。

GUI E2E 在 CI 操作真实控件，Windows 连接实际桌面 WebView2，Linux 操作同一内嵌页面并启动原生窗口做烟测；保存浅色/深色/窄窗口截图、浏览器 trace、状态快照、设置持久化和 SHA-256 报告。原有协议 E2E 使用 `--headless`，仍穿过真实 Rust 服务。
