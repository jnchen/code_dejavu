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

## 发布流程

1. 将版本号同时更新到 `package.json`、`src-tauri/Cargo.toml` 和 `src-tauri/tauri.conf.json`。
2. 更新根目录 `CHANGELOG.md`。
3. 使用上面的环境变量完成正式构建。
4. 在 `https://github.com/jnchen/code_dejavu/releases` 创建 `vX.Y.Z`
   Release，并准备上传 NSIS 安装包、`.sig` 和 `latest.json`。公开文件名统一
   使用 ASCII，例如 `code-dejavu_X.Y.Z_x64-setup.exe`。
5. 用公开下载地址生成更新清单：

   ```powershell
   .\scripts\New-UpdaterManifest.ps1 `
     -DownloadUrl "https://github.com/jnchen/code_dejavu/releases/download/vX.Y.Z/code-dejavu_X.Y.Z_x64-setup.exe" `
     -Notes "本版本的用户可见更新说明"
   ```

6. 将生成的 `latest.json`、安装包和 `.sig` 一起上传到同一个 GitHub Release。客户端读取地址由 `src-tauri/tauri.conf.json` 的 `plugins.updater.endpoints` 指定。
7. 在未登录浏览器中确认以下两个 URL 都能直接访问：
   - `latest.json`
   - `latest.json` 中的安装包 URL
8. 使用旧版本客户端执行“设置 → 检查更新”，完成下载、签名验证、安装和重启的端到端验证。

> Release 只能包含公开构建产物、签名和更新清单，绝不能包含私钥、私钥密码、
> `.env`、本机日志或用户会话数据。
