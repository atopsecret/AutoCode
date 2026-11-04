use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// 粘贴行为模式
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PasteMode {
    /// 智能检测 — 避免与系统 AutoFill 冲突
    Smart,
    /// 总是自动粘贴
    Always,
    /// 只显示悬浮窗，手动点击
    FloatingOnly,
    /// 只复制到剪贴板
    ClipboardOnly,
}

impl Default for PasteMode {
    fn default() -> Self {
        Self::Smart
    }
}

/// 应用配置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppConfig {
    /// 是否监听 iMessage 短信
    pub listen_imessage: bool,
    /// 是否监听 Apple Mail
    pub listen_apple_mail: bool,
    /// 是否监听 Spotlight 邮件源（兼容 Outlook 等客户端）
    pub listen_outlook: bool,
    /// 是否监听邮件客户端的系统通知
    #[serde(default = "default_true")]
    pub listen_notification: bool,
    /// 粘贴行为
    pub paste_mode: PasteMode,
    /// 自动粘贴后是否按回车
    pub auto_enter: bool,
    /// 开机自启
    pub launch_at_login: bool,
    /// Smart 模式下等待 macOS AutoFill 的时间（毫秒）
    pub autofill_detect_delay_ms: u64,
    /// 验证码关键词
    pub verification_keywords: Vec<String>,
    /// 验证码正则表达式列表（模板匹配）
    pub verification_patterns: Vec<String>,
    /// 已知验证码发件人地址（白名单）
    pub known_2fa_senders: Vec<String>,
    /// 系统 AutoFill 原生支持的 App Bundle IDs
    pub native_autofill_apps: Vec<String>,
    /// 配置文件版本
    #[serde(default = "default_version")]
    pub version: u32,
    /// 乐观并发版本，防止设置页覆盖托盘刚保存的配置
    #[serde(default)]
    pub revision: u64,
}

fn default_version() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

const CONFIG_VERSION: u32 = 2;
const LEGACY_DEFAULT_PATTERNS: [&str; 6] = [
    r"(?:验证码|校验码|动态密码|安全码)[：:是为\s]*(\d{4,8})",
    r"(?:verification|confirmation|security|sign-?in|login)\s*code[：:is\s]+([A-Za-z0-9]{4,8})",
    r"(?:Your|The)\s+(?:OTP|code|PIN|token)[：:is\s]+([A-Za-z0-9]{4,8})",
    r"(\d{4,8})\s*(?:是|为|is)?\s*(?:你的|您的|your)\s*(?:验证码|code)",
    r"(\d{4,8})\s*[（(].*(?:有效|valid|expire)",
    r"(?:Security code|安全代码|Sign-in code|登录代码)[：:\s]+(\d{4,8})",
];

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            listen_imessage: true,
            listen_apple_mail: true,
            listen_outlook: true,
            listen_notification: true,
            paste_mode: PasteMode::Smart,
            auto_enter: false,
            launch_at_login: false,
            autofill_detect_delay_ms: 1500,
            verification_keywords: vec![
                "验证码".into(),
                "校验码".into(),
                "动态密码".into(),
                "安全码".into(),
                "verification code".into(),
                "security code".into(),
                "confirmation code".into(),
                "one-time code".into(),
                "OTP".into(),
                "captcha".into(),
                "sign-in code".into(),
                "login code".into(),
                "인증".into(),
            ],
            verification_patterns: vec![
                // 中文模板：验证码是 123456
                r"(?i)(?:验证码|校验码|动态密码|安全码|安全代码|登录代码)[：:是为\s]*([0-9]{4,8})".into(),
                // 英文模板：Your verification code is 123456
                r"(?i)(?:verification|confirmation|security|sign[- ]?in|login|authentication)\s+code\s*(?:is\s*)?[:：]?\s*([A-Za-z0-9]{4,8})".into(),
                // 明确的一次性密码语义，不把普通 code/token 当成验证码
                r"(?i)(?:\bOTP\b|one[- ]time\s+(?:code|password))\s*(?:is\s*)?[:：]?\s*([A-Za-z0-9]{4,8})".into(),
                // 反向：123456 是你的验证码
                r"(?i)([0-9]{4,8})\s*(?:是|为|is)?\s*(?:你的|您的|your)\s*(?:验证码|verification\s+code|security\s+code|sign[- ]?in\s+code)".into(),
                // 动作模板：Please use 123456 to sign in
                r"(?i)\b(?:use|enter)\s+([A-Za-z0-9]{4,8})\s+to\s+(?:sign\s*in|log\s*in|verify|authenticate)\b".into(),
            ],
            known_2fa_senders: vec![
                "noreply@microsoft.com".into(),
                "account-security-noreply@accountprotection.microsoft.com".into(),
                "no-reply@accounts.google.com".into(),
                "noreply@github.com".into(),
                "verify@twitter.com".into(),
                "noreply@apple.com".into(),
                "noreply@steam.com".into(),
            ],
            native_autofill_apps: vec![
                "com.apple.Safari".into(),
                "com.apple.systempreferences".into(),
                "com.apple.AppStore".into(),
            ],
            version: CONFIG_VERSION,
            revision: 0,
        }
    }
}

impl AppConfig {
    /// 获取配置文件路径
    pub fn config_path() -> Result<PathBuf> {
        let dir = dirs::config_dir()
            .context("无法获取系统配置目录")?
            .join("autocode");
        Ok(dir.join("config.toml"))
    }

    /// 获取日志目录
    #[allow(dead_code)]
    pub fn log_dir() -> Result<PathBuf> {
        let dir = dirs::config_dir()
            .context("无法获取系统配置目录")?
            .join("autocode")
            .join("logs");
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// 验证配置的有效性
    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            anyhow::bail!(
                "不支持配置文件版本 {}，当前版本为 {}",
                self.version,
                CONFIG_VERSION
            );
        }
        // 验证延迟时间范围（100ms - 10000ms）
        if self.autofill_detect_delay_ms < 100 || self.autofill_detect_delay_ms > 10000 {
            anyhow::bail!(
                "autofill_detect_delay_ms 必须在 100-10000 之间，当前值: {}",
                self.autofill_detect_delay_ms
            );
        }

        // 验证正则表达式的有效性
        if self.verification_patterns.is_empty() || self.verification_patterns.len() > 128 {
            anyhow::bail!("verification_patterns 必须包含 1-128 个模板");
        }
        for (idx, pattern) in self.verification_patterns.iter().enumerate() {
            if pattern.is_empty() || pattern.len() > 4096 {
                anyhow::bail!("正则表达式 #{} 长度必须在 1-4096 字节之间", idx + 1);
            }
            let regex = regex::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .size_limit(2 * 1024 * 1024)
                .build()
                .with_context(|| format!("正则表达式 #{} 无效: {}", idx + 1, pattern))?;
            if regex.captures_len() < 2 {
                anyhow::bail!("正则表达式 #{} 必须有一个验证码捕获组", idx + 1);
            }
        }

        validate_items(
            "verification_keywords",
            &self.verification_keywords,
            128,
            false,
        )?;
        validate_items("known_2fa_senders", &self.known_2fa_senders, 254, true)?;
        validate_items(
            "native_autofill_apps",
            &self.native_autofill_apps,
            255,
            true,
        )?;
        for address in &self.known_2fa_senders {
            if address.matches('@').count() != 1
                || address.starts_with('@')
                || address.ends_with('@')
                || address
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '*' | ',' | ';'))
            {
                anyhow::bail!("白名单必须是完整邮件地址，不支持通配符: {}", address);
            }
        }

        Ok(())
    }

    /// 从磁盘加载配置
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            let config = Self::default();
            config.save()?;
            log::info!("创建了默认配置文件: {:?}", path);
            return Ok(config);
        }

        let content =
            fs::read_to_string(&path).with_context(|| format!("读取配置文件失败: {:?}", path))?;

        Self::from_toml(&content)
    }

    /// 纯解析及迁移；不在加载时覆写用户配置或备份
    pub fn from_toml(content: &str) -> Result<Self> {
        let values: toml::Value = toml::from_str(content).context("解析配置文件失败")?;
        let mut config: Self = toml::from_str(content).context("解析配置文件失败")?;
        if config.version < CONFIG_VERSION {
            if values.get("listen_notification").is_none() {
                // 旧版通知与 Spotlight 共用开关，迁移时保留用户的关闭选择
                config.listen_notification = config.listen_outlook;
            }
            if config
                .verification_patterns
                .iter()
                .map(String::as_str)
                .eq(LEGACY_DEFAULT_PATTERNS)
            {
                config.verification_patterns = Self::default().verification_patterns;
            }
            config.version = CONFIG_VERSION;
        }
        config.validate()?;
        Ok(config)
    }

    /// 保存配置到磁盘
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::config_path()?)
    }

    /// 先完整写入同目录临时文件，再原子替换；失败不破坏现有配置
    fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let name = path
            .file_name()
            .context("配置文件名为空")?
            .to_string_lossy();
        let temp = path.with_file_name(format!(".{}.{}.{}.tmp", name, std::process::id(), nonce));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).context("创建配置临时文件失败")?;
        file.write_all(content.as_bytes())
            .context("写入配置临时文件失败")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path).context("原子保存配置失败；原配置未被清空")?;
        Ok(())
    }
}

fn validate_items(
    name: &str,
    items: &[String],
    max_length: usize,
    allow_empty: bool,
) -> Result<()> {
    if (!allow_empty && items.is_empty()) || items.len() > 128 {
        anyhow::bail!("{} 的项目数量不合法（最多 128 个）", name);
    }
    if items
        .iter()
        .any(|item| item.trim().is_empty() || item.len() > max_length || item.contains('\0'))
    {
        anyhow::bail!("{} 包含空项目、过长项目或 NUL 字符", name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_valid_captures_and_current_version() {
        let config = AppConfig::default();
        config.validate().unwrap();
        assert_eq!(config.version, 2);
        assert!(config.listen_notification);
    }

    #[test]
    fn legacy_defaults_migrate_without_reenabling_disabled_sources() {
        let mut config = AppConfig::default();
        config.version = 1;
        config.listen_outlook = false;
        config.verification_patterns = LEGACY_DEFAULT_PATTERNS
            .iter()
            .map(|p| (*p).into())
            .collect();
        let value = toml::to_string(&config)
            .unwrap()
            .lines()
            .filter(|line| {
                !line.starts_with("listen_notification =") && !line.starts_with("revision =")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let migrated = AppConfig::from_toml(&value).unwrap();
        assert!(!migrated.listen_notification);
        assert_eq!(
            migrated.verification_patterns,
            AppConfig::default().verification_patterns
        );
        assert_eq!(migrated.revision, 0);
    }

    #[test]
    fn migration_preserves_custom_templates() {
        let mut config = AppConfig::default();
        config.version = 1;
        config.verification_patterns = vec![r"验证码是([0-9]{6})".into()];
        let migrated = AppConfig::from_toml(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(migrated.verification_patterns, config.verification_patterns);
    }

    #[test]
    fn invalid_templates_and_blank_keywords_are_rejected() {
        let mut config = AppConfig::default();
        config.verification_patterns = vec!["[0-9]{6}".into()];
        assert!(config.validate().is_err());
        config = AppConfig::default();
        config.verification_keywords = vec![" ".into()];
        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_save_keeps_previous_file_untouched() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("autocode-config-{}-{}", std::process::id(), nonce))
            .join("config.toml");
        let mut config = AppConfig::default();
        config.save_to(&path).unwrap();
        let before = fs::read(&path).unwrap();
        config.verification_patterns.clear();
        assert!(config.save_to(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

/// 线程安全的全局配置
pub type SharedConfig = Arc<RwLock<AppConfig>>;

/// 创建共享配置
pub fn load_shared_config() -> Result<SharedConfig> {
    let config = AppConfig::load()?;
    Ok(Arc::new(RwLock::new(config)))
}
