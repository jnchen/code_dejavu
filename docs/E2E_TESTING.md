# E2E 测试

Tauri 可以做 E2E。本项目分两层测试，避免任何一层接触真实用户记录。

## 1. 前端 E2E（Playwright，推荐日常运行）

使用真实 SvelteKit 页面，但把 Tauri IPC 替换为内存夹具：

```powershell
pnpm test:e2e
```

- 测试文件：`tests/e2e/`
- IPC 夹具：`tests/e2e/mock-tauri.ts`
- 服务：Playwright 自动启动 `pnpm dev`
- 覆盖：
  - 控制台渲染
  - 会话列表、详情、工具层级、子代理展开
  - 全局会话搜索、深度全文搜索、收藏元数据
  - 会话详情分页加载
  - 8 个主页面渲染冒烟
  - 快照创建 / 恢复 / 删除
  - 记忆编辑 / 删除
  - 规则启停
  - 指令保存
  - 设置保存、价格刷新、更新检查（无更新路径）
  - Codex 关联进程查看与关闭
  - WSL 主机徽标与非本机路径显示
  - 多级（祖→父→孙）子代理展开
  - 中英文切换
  - 未知命令隔离
- **不访问磁盘**：mock 只返回内存对象，`invoke()` 不会到达 Rust 后端，因此
  不会读取 `~/.claude`、`~/.codex`、OpenCode 数据库或 Pi 会话。

## 2. 原生 Tauri E2E（嵌入式 WebDriver）

使用真实 Tauri 窗口、真实 Rust 后端和真实 WebView2/WKWebView/WebKitGTK：

```powershell
pnpm test:e2e:native:build
```

之后可以重复运行而不重新构建：

```powershell
pnpm test:e2e:native
```

实现方式：

- Rust `e2e` feature 启用 `tauri-plugin-wdio-webdriver`，在应用内启动 W3C
  WebDriver HTTP 服务（默认端口 4445）。**不要在生产构建启用这个 feature。**
- `tests/e2e-native/run.mjs` 启动应用并直接通过 WebDriver 协议操作页面。
- 测试文件：`tests/e2e-native/`
  - `seed.mjs`：创建一次性 home
  - `run.mjs`：启动应用、执行断言、结束后清理

覆盖内容：

- 控制台真实索引完成并渲染
- Claude 会话从 JSONL 解析后出现在列表
- Codex rollout 会话解析并打开
- Pi 追加式会话树解析并打开
- Claude 两级嵌套子代理逐层展开
- OpenCode 同一数据库中的 **v2 会话**与**仅存在于 v1 表的旧会话**同时出现
- 打开 v2 会话后消息、工具调用正常渲染
- v2 会话内搜索

### 隔离方式

`run.mjs` 启动应用时覆盖以下环境变量，全部指向 `.tmp-e2e/fixtures`：

| 变量 | 用途 |
| --- | --- |
| `USERPROFILE` / `HOME` | Claude、Codex、OpenCode、Pi 的 home |
| `APPDATA` | `CodeDejavu/config.json` |
| `LOCALAPPDATA` | 应用索引、归档等数据目录 |
| `XDG_DATA_HOME` 等 | Linux/macOS 路径 |

同时写入 `wsl_scan: false`，避免扫描真实 WSL 发行版。测试结束后删除整个夹具目录。

### 调试

```powershell
$env:E2E_KEEP = "1"
pnpm test:e2e:native
```

失败时会保留 `.tmp-e2e/fixtures`，并输出页面 HTML 与应用日志。

## 注意事项

- 原生 E2E 需要先构建前端和启用 `e2e` feature 的应用：
  `pnpm build && pnpm tauri build --debug --no-bundle --features e2e`
- 普通 `cargo build` 的 debug 二进制会使用 `devUrl`，不适合原生 E2E；
  使用上面的 `tauri build --debug` 才会打包 `build/` 前端资源。
- 发布构建不要带 `--features e2e`。
