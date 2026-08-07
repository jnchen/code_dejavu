# 参与贡献

感谢参与 Code Déjà Vu。

1. 从 `main` 创建主题分支。
2. 保持改动聚焦，不要提交构建产物、日志、真实会话或任何凭据。
3. 提交前运行：

   ```powershell
   pnpm check
   pnpm build
   cargo test --manifest-path src-tauri/Cargo.toml
   ```

4. 在 Pull Request 中说明行为变化、验证方式，以及涉及本地数据时的回滚方法。

新增 provider 时，请同时参考 [`docs/agent-capability-model.md`](docs/agent-capability-model.md)
中的能力模型和 UI 契约。
