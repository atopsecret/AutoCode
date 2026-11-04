//! 纯决策与短期去重。不接触剪贴板、键盘或用户数据库。
use std::collections::{BTreeSet, VecDeque};

pub const MAX_MESSAGE_AGE_SECONDS: f64 = 120.0;
const DUPLICATE_WINDOW_SECONDS: f64 = 30.0;
const MAX_RECENT_CODES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetMetadata {
    pub pid: i32,
    pub role: String,
    pub hint: String,
    pub enabled: bool,
    pub editable: bool,
}

impl TargetMetadata {
    /// 不把普通密码框、聊天框、终端或大文本域当成验证码输入框。
    pub fn is_verification_field(&self) -> bool {
        if self.pid <= 0 || !self.enabled || !self.editable || self.role != "AXTextField" {
            return false;
        }
        let hint = self.hint.to_lowercase();
        let terms = [
            "验证码",
            "校验码",
            "动态密码",
            "安全代码",
            "verification code",
            "verification-code",
            "verification_code",
            "verificationcode",
            "one-time",
            "one time",
            "security code",
            "confirmation code",
            "2fa",
            "two-factor",
            "인증",
        ];
        terms.iter().any(|term| hint.contains(term))
            || hint
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|s| s == "otp")
    }
}

pub fn message_is_fresh(received_at: f64, now: f64) -> bool {
    received_at.is_finite()
        && now.is_finite()
        && received_at <= now + 5.0
        && now - received_at <= MAX_MESSAGE_AGE_SECONDS
}

/// 只使用实际邮件地址与浏览器报告的 HTTPS 主机，不从正文猜服务归属。
pub fn target_matches_sender(sender: Option<&str>, host: Option<&str>) -> bool {
    let (Some(sender), Some(host)) = (sender, host) else {
        return false;
    };
    let sender = sender.trim().to_ascii_lowercase();
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if sender.matches('@').count() != 1
        || sender
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';'))
    {
        return false;
    }
    let domain = sender.split('@').nth(1).unwrap_or("");
    let within = |value: &str, root: &str| {
        value == root
            || value
                .strip_suffix(root)
                .is_some_and(|prefix| prefix.ends_with('.'))
    };
    if domain.is_empty() || host.is_empty() {
        return false;
    }
    if within(&host, domain) {
        return true;
    }
    let microsoft = ["microsoft.com", "microsoftonline.com", "live.com"];
    let google = ["google.com"];
    let apple = ["apple.com", "icloud.com"];
    [microsoft.as_slice(), google.as_slice(), apple.as_slice()]
        .iter()
        .any(|roots| {
            roots.iter().any(|root| within(domain, root))
                && roots.iter().any(|root| within(&host, root))
        })
}

#[derive(Debug)]
struct RecentCode {
    id: String,
    code: String,
    sender: Option<String>,
    context: BTreeSet<String>,
    at: f64,
    received_at: f64,
}

#[derive(Default)]
pub struct DeliveryDeduplicator {
    recent: VecDeque<RecentCode>,
}

fn context_tokens(text: &str) -> BTreeSet<String> {
    let stop = [
        "your",
        "the",
        "is",
        "code",
        "verification",
        "security",
        "login",
        "sign",
        "in",
        "otp",
        "pin",
        "this",
        "for",
        "and",
        "please",
        "do",
        "not",
        "share",
        "with",
        "anyone",
        "valid",
        "minutes",
        "minute",
        "synthetic",
        "welcome",
        "hello",
        "account",
        "email",
        "from",
        "new",
        "subject",
        "message",
        "dear",
        "验证码",
        "校验码",
        "安全码",
        "请勿泄露",
        "请勿分享",
    ];
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() > 2 && !s.chars().any(|c| c.is_ascii_digit()) && !stop.contains(s))
        .take(128)
        .map(str::to_owned)
        .collect()
}

impl DeliveryDeduplicator {
    /// 源内按消息 ID 去重；跨源只在短窗口内合并同码及相符的发送方/上下文。
    /// 没有发送方也没有服务语义时，只能按同码短时合并，不长期封禁同一验证码。
    pub fn accept(
        &mut self,
        id: &str,
        code: &str,
        sender: Option<&str>,
        text: &str,
        received_at: f64,
        now: f64,
    ) -> bool {
        self.recent
            .retain(|e| now - e.at <= MAX_MESSAGE_AGE_SECONDS + 5.0 && now >= e.at);
        let sender = sender.map(|s| s.trim().to_lowercase());
        let context = context_tokens(text);
        let duplicate = self.recent.iter().any(|e| {
            if !id.is_empty() && e.id == id {
                return true;
            }
            let same_context = if let (Some(a), Some(b)) = (&e.sender, &sender) {
                a == b
            } else {
                context.is_empty()
                    || e.context.is_empty()
                    || context.is_subset(&e.context)
                    || e.context.is_subset(&context)
            };
            same_context
                && (received_at < e.received_at
                    || (e.code == code && now - e.at <= DUPLICATE_WINDOW_SECONDS))
        });
        if duplicate {
            return false;
        }
        if self.recent.len() == MAX_RECENT_CODES {
            self.recent.pop_front();
        }
        self.recent.push_back(RecentCode {
            id: id.to_owned(),
            code: code.to_owned(),
            sender,
            context,
            at: now,
            received_at,
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(role: &str, hint: &str) -> TargetMetadata {
        TargetMetadata {
            pid: 7,
            role: role.into(),
            hint: hint.into(),
            enabled: true,
            editable: true,
        }
    }

    #[test]
    fn target_requires_verified_editable_otp_field() {
        assert!(target("AXTextField", "Your verification code").is_verification_field());
        assert!(target("AXTextField", "otp-input").is_verification_field());
        assert!(!target("AXTextField", "Password").is_verification_field());
        assert!(!target("AXTextArea", "verification code").is_verification_field());
        assert!(!target("AXTextField", "adoption").is_verification_field());
        assert!(!target("AXTextField", "Email address for verification").is_verification_field());
        let mut disabled = target("AXTextField", "验证码");
        disabled.enabled = false;
        assert!(!disabled.is_verification_field());
    }

    #[test]
    fn autofill_requires_matching_service_not_only_an_otp_field() {
        assert!(target_matches_sender(
            Some("noreply@github.com"),
            Some("github.com")
        ));
        assert!(target_matches_sender(
            Some("noreply@accounts.google.com"),
            Some("accounts.google.com")
        ));
        assert!(target_matches_sender(
            Some("noreply@microsoft.com"),
            Some("login.live.com")
        ));
        assert!(!target_matches_sender(
            Some("noreply@github.com"),
            Some("accounts.google.com")
        ));
        assert!(!target_matches_sender(
            Some("noreply@github.com"),
            Some("github.com.evil.invalid")
        ));
        assert!(!target_matches_sender(
            Some("noreply@github.com.evil.invalid"),
            Some("github.com")
        ));
        assert!(!target_matches_sender(None, Some("github.com")));
        assert!(!target_matches_sender(Some("10690000"), Some("github.com")));
    }

    #[test]
    fn late_or_future_messages_do_not_replay() {
        assert!(message_is_fresh(100.0, 101.0));
        assert!(!message_is_fresh(100.0, 221.0));
        assert!(!message_is_fresh(110.0, 100.0));
        assert!(!message_is_fresh(f64::NAN, 100.0));
    }

    #[test]
    fn same_code_from_three_sources_is_delivered_once() {
        let mut d = DeliveryDeduplicator::default();
        assert!(d.accept(
            "mail:1",
            "123456",
            Some("noreply@example.invalid"),
            "Your verification code is 123456",
            100.0,
            100.0
        ));
        assert!(!d.accept(
            "spotlight:1",
            "123456",
            Some("noreply@example.invalid"),
            "Your verification code is 123456",
            100.0,
            101.0
        ));
        assert!(!d.accept(
            "notification:1",
            "123456",
            None,
            "Your verification code is 123456",
            100.0,
            102.0
        ));
    }

    #[test]
    fn separate_services_and_future_reuse_are_not_suppressed() {
        let mut d = DeliveryDeduplicator::default();
        assert!(d.accept(
            "mail:1",
            "123456",
            Some("noreply@first.invalid"),
            "First verification code 123456",
            100.0,
            100.0
        ));
        assert!(d.accept(
            "mail:2",
            "123456",
            Some("noreply@second.invalid"),
            "Second verification code 123456",
            101.0,
            101.0
        ));
        assert!(d.accept(
            "notification:3",
            "123456",
            None,
            "First verification code 123456",
            132.0,
            132.0
        ));
    }

    #[test]
    fn older_code_arriving_after_newer_does_not_override_it() {
        let mut d = DeliveryDeduplicator::default();
        assert!(d.accept(
            "mail:new",
            "222222",
            Some("noreply@example.invalid"),
            "Example verification code",
            102.0,
            103.0
        ));
        assert!(!d.accept(
            "notification:old",
            "111111",
            None,
            "Example verification code",
            101.0,
            104.0
        ));
        assert!(!d.accept(
            "notification:late-old",
            "111111",
            None,
            "Example verification code",
            101.0,
            144.0
        ));
    }

    #[test]
    fn generic_welcome_does_not_merge_different_services() {
        let mut d = DeliveryDeduplicator::default();
        assert!(d.accept(
            "notification:1",
            "123456",
            None,
            "Welcome Firstservice verification code",
            100.0,
            100.0
        ));
        assert!(d.accept(
            "notification:2",
            "123456",
            None,
            "Welcome Secondservice verification code",
            101.0,
            101.0
        ));
    }
}
