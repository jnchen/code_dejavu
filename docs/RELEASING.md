# 发布与在线更新

Code Déjà Vu 使用 Tauri v2 官方 Updater。客户端只安装通过内置公钥验证的更新包。

## 本机签名材料

首次接入时已在当前 Windows 用户目录生成：

- 私钥：`%USERPROFILE%\.tauri\code-dejavu-updater.key`
- 私钥密码：`%USERPROFILE%\.tauri\code-dejavu-updater.password`
- 公钥：`%USERPROFILE%\.tauri\code-dejavu-updater.key.pub`

私钥和密码不能提交到 Git。请将这两个文件备份到密码管理器或其他安全位置；丢失后，已安装的客户端将无法验证由新密钥签发的更新。

## 构建签名更新包

```powershell
$env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content -Raw "$HOME\.tauri\code-dejavu-updater.key").Trim()
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = (Get-Content -Raw "$HOME\.tauri\code-dejavu-updater.password").Trim()
pnpm tauri build
```

Windows 构建会生成安装包及相应 `.sig` 文件。发布时必须同时保存签名文件。

## 配置 GitHub Actions 签名

仓库的 `Release desktop apps` 工作流使用 Windows 和 macOS runner 分别构建 x64 NSIS
安装包及 Apple Silicon DMG。两个平台的 updater 包必须使用与客户端内置公钥匹配的同
一把 Tauri 私钥签名。首次使用前，从保存私钥的 Windows 机器执行：

```powershell
$privateKey = (Get-Content -Raw "$HOME\.tauri\code-dejavu-updater.key").Trim()
$privateKeyPassword = (Get-Content -Raw "$HOME\.tauri\code-dejavu-updater.password").Trim()
gh secret set TAURI_SIGNING_PRIVATE_KEY --body $privateKey
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --body $privateKeyPassword
Remove-Variable privateKey, privateKeyPassword
```

可用 `gh secret list` 确认两个 secret 名称已经存在。GitHub 只显示名称和更新时间，不会
返回 secret 内容。不要生成新密钥代替现有密钥，否则已经安装的客户端无法验证更新。

## 从 Windows 触发跨平台发布

Windows 不能在本机使用 Tauri 官方工具链生成 macOS `.app` / `.dmg`。推送版本 tag 后，
GitHub Actions 会自动在各自系统上完成测试、打包、签名和发布：

```powershell
git tag vX.Y.Z
git push origin main vX.Y.Z
gh run watch
```

也可以对已经存在的 tag 手动重新运行：

```powershell
gh workflow run release-desktop.yml -f release_tag=vX.Y.Z
gh run watch
```

工作流会先校验 tag 与 `package.json`、`src-tauri/Cargo.toml`、
`src-tauri/tauri.conf.json` 中的版本完全一致。Windows 和 macOS 构建都会运行前端检查及
Rust 测试；任何一个任务失败都不会创建新 Release。CI 测试使用仓库内的 fixture，不会
读取开发者电脑上的真实会话，因此发布后仍需按下文步骤做一次旧版本升级验证。

同一个 Release 最终包含：

- Windows x64 NSIS 安装包及 `.sig`
- macOS arm64 DMG、`.app.tar.gz` updater 包及 `.sig`
- `latest.json`，含 `windows-x86_64` 和 `darwin-aarch64`
- 三个可安装/更新包的 `SHA256SUMS.txt`

macOS 应用目前使用 ad-hoc 签名，不需要 Apple Developer 凭据，但没有经过 Apple
公证，首次从 DMG 安装时可能需要用户在系统设置中确认打开。Tauri updater 包仍由上述
私钥签名，和 Apple 应用签名是两套独立机制。面向普通用户正式分发时，应再接入
Developer ID Application 证书和 Apple 公证。

## 发布流程

1. 将版本号同时更新到 `package.json`、`src-tauri/Cargo.toml` 和 `src-tauri/tauri.conf.json`。
2. 更新根目录 `CHANGELOG.md`。
3. 确认上面的两个 GitHub Actions secret 已配置，然后提交版本变更。
4. 创建并推送 `vX.Y.Z` tag。工作流会生成并发布所有安装包、签名和
   `latest.json`；公开文件名统一使用 ASCII。
5. 在未登录浏览器中确认以下 URL 都能直接访问：
   - `latest.json`
   - `latest.json` 中 Windows 与 macOS 的两个 updater URL
6. 分别使用旧版 Windows 和 macOS 客户端执行“设置 → 检查更新”，完成下载、签名
   验证、安装和重启的端到端验证。

> Release 只能包含公开构建产物、签名和更新清单，绝不能包含私钥、私钥密码、
> `.env`、本机日志或用户会话数据。
