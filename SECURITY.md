# 安全策略

## 报告漏洞

请不要在公开 Issue 中披露未修复的漏洞、凭据、真实会话内容或可识别个人身份的数据。
优先使用 GitHub 的
[Private vulnerability reporting](https://github.com/jnchen/code_dejavu/security/advisories/new)
提交报告，并包含影响范围、复现步骤和建议修复方式。

## 数据边界

Code Déjà Vu 会读取本机 coding agent 的会话与配置目录。安全问题复现时请使用脱敏样本，
不要上传真实用户目录、认证文件、API key、Cookie、签名私钥或完整会话归档。
