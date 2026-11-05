# 仓库迁移记录

日期：2026-11-05。新仓库：[atopsecret/AutoCode](https://github.com/atopsecret/AutoCode)。

## 源码与历史

本地原副本是浅克隆，最早保存的 `9b3b2823f9020b66a9f4f5bf995d10cd72d53c91` 缺少父提交 `353174c384c532bbf3ed871f3f66c2673fc56983`。旧仓库访问受限后，直接推送该浅克隆被 GitHub 拒绝。

通过 [itsharex/AutoCode](https://github.com/itsharex/AutoCode) 补齐历史。GitHub API 确认该仓库的 `parent` 和 `source` 均为原 `skernelx/AutoCode`，精确父提交与原边界提交的 SHA、tree、parent 均匹配。补齐后仓库不再是浅克隆，完整对象检查通过；没有拼接同名无关仓库，也没有重写原提交或版本标签。

`main` 增加了新仓库地址、安装说明和链接回归测试。`v0.6.0` 仍指向原发布提交 `d57fb5ab1ff9f82656db30d0c97d61de241a8c96`。原有本地分支、五个版本标签及原 Git 备份保留。

## 恢复的发布包

- `AutoCode_0.6.0_arm64.app.zip`：原 Apple Silicon App ZIP，内容未改。
- `AutoCode_0.6.0_source_d57fb5ab.zip`：原发布提交的源码 ZIP，144 个文件与 `v0.6.0` 逐项一致。
- `SHA256SUMS`：上述两个 ZIP 的原 SHA-256 校验清单。

两个 ZIP 均完成 CRC 与 SHA-256 回读；App 完成 43 项静态检查，包括版本、架构、代码签名和 dyld 加载布局。此次迁移没有启动 App，没有读取真实消息数据库或测试真实辅助功能填入。

App 包使用 ad-hoc 签名，没有 Developer ID 签名或 Apple 公证。安装方式仍是手动解压；没有签名 Tauri 更新包，也没有 `latest.json`。旧安装包内部的原更新地址不被二进制补丁改写；后续源码地址已改为新仓库。

## 自动化与旧平台数据

迁移期间新仓库 Actions 保持关闭，不运行自动构建、搜索或发布。工作流源码作为项目文件保留，是否重新启用由仓库所有者决定。

新建仓库不继承旧仓库的 Stars、Issues、Pull Requests、Actions 记录或 GitHub Secrets。旧 Actions URL 仅是历史出处，不代表新仓库已经运行过这些任务。
