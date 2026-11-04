# AutoCode 0.6.0 修复说明

这是未发布的本地修复，附已构建的 arm64 App ZIP。基线来自 2026-03-13 恢复的 0.5.0 工作树；不能证明与 3 月 6 日发布二进制完全一致。恢复原件、ZIP 备份和当前安装 App 均保留。

## 行为变化

- 四个来源独立启停和报告状态。授权重新检测会重试失败来源；一个来源缺目录、读取被拒绝或崩溃，不阻止其他来源。异常有退避重试，panic 不再留下假绿灯。
- 通知使用动态路径发现、schema 检查、应用标识关联和 plist 正文解码。分页、成功处理进度和短时间重叠窗口防止查询失败及延迟入库漏读。
- Apple Mail 解码 MIME；Spotlight 未取到正文时保留重试。来源内按消息身份去重，消费者再做短时跨来源合并，防止重复输入及迟到旧码覆盖。
- 普通订单号、HTML 加粗数字或白名单发件人不再单独构成验证码证据。多码歧义不会自动填入。
- 自动填入只允许唯一明确的验证码、可编辑 OTP 字段、未变化的焦点及与邮件发送方匹配的 HTTPS 服务。无法确认浏览器来源、消息没有可核验邮件域或只有普通文本框时，显示浮窗供用户点击复制。不会向全局键盘发送文字或回车。
- Smart 模式仍优先避免系统 AutoFill 冲突。“仅浮窗”不自动复制；“仅复制”不弹浮窗。自动回车在本版暂停，请手动提交。
- 设置页采用配置版本和未保存字段合并。托盘修改不被旧页面覆盖，保存冲突、来源故障和启动配置错误可见。
- 更新由 Rust 后端调用官方 updater。没有公钥时拒绝下载/安装；发布清单必须来自真实 `.app.tar.gz` 和签名 sidecar，SHA-256 不是签名。

## 运行测试

依赖已缓存时，一次运行全部离线回归：`bash scripts/test-local.sh`。单项命令如下。

```sh
npm run test:ui
npm run test:update-manifest
python3 tests/notification_sql.py
cargo test --offline --locked --manifest-path tests/core-harness/Cargo.toml --lib
cargo test --offline --locked --manifest-path tests/ingestion-harness/Cargo.toml --lib
cargo test --offline --locked --manifest-path tests/focus-harness/Cargo.toml --lib
```

三个 Rust 测试包直接引用生产源码。输入、文件和数据库均为合成 fixture；focus 测试包编译/链接 AX/CF 封装但不调用系统输入或读取焦点。

## 完整构建与启动

需要 macOS、Rust、Node.js、Python 3.11+ 和 Command Line Tools。应用版本为 `0.6.0`，六处版本值有一致性检查。执行完整构建：

```sh
python3 scripts/build-local.py
# 或 npm run build
# 只需要 App/ZIP、不需要 DMG：
python3 scripts/build-local.py --bundles app
# 依赖已完整缓存时可使用 --offline；缺依赖会失败，不会替换依赖版本。
```

入口优先使用版本匹配的本地 `cargo-tauri`；本机已实测为 `2.10.1`。没有本地 CLI 时，仅按 `package-lock.json` 安装项目依赖，不全局安装。所有 Cargo 命令使用 `--locked`。

本机 release 过程宏和主程序曾报 `mis-aligned LINKEDIT string pool`，过程宏加载失败进一步表现为 `can't find crate for zerofrom_derive`。项目在 `[profile.release]` 中明确设置 `strip = "none"`，关闭符号裁剪，保留默认 release 优化。已用真实 `zerofrom` 编译、动态库加载和 `dyld_info` 验证定位；没有删除 Cargo 缓存、升级依赖或修改全局工具链。

构建依次执行源码校验、回归测试、完整 crate 单测、全部 target 类型检查、release App/DMG 打包、App 静态验证、ZIP 打包与普通文件 SHA-256 回读。任一步失败立即停止；下载、测试和编译均限时，普通失败、超时或取消会清理本次创建的子进程组。报告和日志写到 `artifacts/build-0.6.0/<本轮目录>/`，每轮保留，不覆盖上一轮。

静态验证检查实际 App 的版本、Bundle ID、可执行文件、最低系统、`LSUIElement`、Resources 图标、Mach-O 架构、`dyld_info -validate_only` 加载布局、严格 codesign 验证和 entitlement。ZIP 回读检查 CRC 及普通文件内容，不代表真机功能验收或 symlink 专项验证。单独验证：

```sh
python3 scripts/verify-build.py --source-only
python3 scripts/verify-build.py \
  --app src-tauri/target/release/bundle/macos/AutoCode.app
```

源码检查通过不等于 App 构建通过；缺少 App 时产物校验必须失败。不会回退读取 `/Applications/AutoCode.app`。CI 也调用完整流水线并保存 App ZIP/报告，但没有推送或触发。

构建入口不会启动 App、安装到 `/Applications` 或发布 Release。需要真机验收时再自行打开新产物；启动会读取已启用的真实来源，不属于合成测试。本轮没有启动 App。

普通 App 使用本地 ad-hoc 签名，与签名更新发布不同。正式更新还需要真实公钥、私钥和发布专用配置中的 `bundle.createUpdaterArtifacts=true`，生成 `.app.tar.gz`/`.sig`；本轮没有启用更新产物或创建密钥。[Tauri 更新构建说明](https://v2.tauri.app/plugin/updater/#building)

## 尚未验证及支持边界

- 2026-11-04 用户补齐依赖后，557 个锁定 registry 包的缓存均通过 SHA-256 校验。已离线执行完整 App 构建；61 个生产 Rust 单测通过，其中包含此前未执行的 12 个通知/iMessage 测试。UI 17 项、构建相关 Python 测试 60 项、更新清单 8 项及通知 SQL/模型 8 项均通过；窄 Rust harness 与生产单测有重叠，不相加宣称独立总数。
- 最终 App 的 43 项源码与产物检查通过，包括 dyld 加载布局、严格签名验证、版本和 entitlement。ZIP 经 CRC、全部普通文件 SHA-256 和归档可执行文件回读校验。交付目录为 `artifacts/build-0.6.0/20261104T131230Z-nynfbtgs/`；前一轮 `20261104T130704Z-7mna_ixt` 的 ZIP 发现加载布局错误，已标记 `DO_NOT_USE.txt`，仅保留作诊断。
- DMG 未生成：实际打包报 `hdiutil: create failed - 设备未配置`，`diskutil` 也报告当前执行环境无法使用 DiskManagement 框架。本机可用的 App/ZIP 构建命令为 `python3 scripts/build-local.py --offline --bundles app`，默认 App+DMG 路径仍受此限制；没有把 DMG 失败当成成功。
- 本机真实通知 schema、AX 字段与真机监控尚未验证；未读取个人消息正文。
- 只有 NSAttributedString typedstream 正文的 Messages 记录仍不支持，明确报告错误，不从归档字节猜码。
- MIME 支持 UTF-8、ASCII、Latin1；其他字符集明确报错。
- 未设置发布公钥或生成私钥。历史假签名清单仅保存在 `tests/fixtures/historical-latest.json`；根 `latest.json` 是签名脚本的生成产物，不提交。

浮窗高度从 190 调整为 260，避免默认布局把复制按钮挤到下方；这是静态布局修正，仍需真机确认。

本机 arm64 完整 App 构建已通过。正式发布前还需用户自选测试账号的真机验收及正式签名发布；其他架构未在本轮构建。当前事实和本轮报告路径见 `REPAIR_RESULT.json`。
