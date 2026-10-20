- NEVER write unit tests after you write code. 
- Highly prefer E2E tests as the sole testing mechanism. Use them to verify complex features work. At the end of E2E tests, produce a verifiable and repeatable artifact. 
- If you must test a system in isolation, FIRST write all the ways it could fail, THEN write the code.

## 开发环境与验证

- 本机仅负责代码编写，不安装 Rust 工具链。
- Rust 编译、格式检查、静态检查和测试交由 GitHub Actions 执行。

## Rust 与代码规范

- Rust 工具链和依赖/各种工具均使用最新稳定版。
- 编写 Rust 代码前，参考 `D:\work\edition-guide` 中的 Rust 2024 现代推荐写法。
- 使用尽量少的代码实现完整需求，优先选择简洁、清晰、优雅的方案。
- 文档优先更新已有文件，仅在用户要求或确有必要时新增 Markdown 文件；不为每次修改单独创建计划、总结或报告。
- `README.md` 刻意保持极简。**默认不要改它，能不改就不改。**

## 网关修改与发布

- 遵循最小修改原则，仅针对当前用户报告的单个问题进行必要修改，避免过度防御。
- 网关对请求和响应的处理应该学习pi的成熟经验
- 阶段性修改完成后直接提交并推送到 `main`；由 GitHub Actions 在全部检查通过后自动创建 tag 和 Release。

## 工作区有pi和客户端的代码
- 评估上游对客户端请求接受度需要看pi的源码
- 评估客户端对上游响应接受度需要看客户端的源码

## diff功能需要保持一个字符的修改都要展示（哪怕已知）
- 指的是项目的diff功能，不是你每次修改需要展示项目diff给用户
