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
