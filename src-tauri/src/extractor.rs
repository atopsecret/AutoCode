use regex::{Captures, Regex, RegexBuilder};
use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::config::AppConfig;

/// 提取到的验证码
#[derive(Debug, Clone)]
pub struct VerificationCode {
    pub code: String,
    pub source: String,
    pub confidence: f32,
    /// 只有唯一、明确关联验证码语义的模板候选才能进入自动输入。
    /// confidence 是策略等级，不是正确概率，也不能单独授权自动输入。
    pub auto_fill_eligible: bool,
}

#[derive(Debug)]
struct Anchor {
    start: usize,
    end: usize,
    strong: bool,
}

fn case_insensitive_regex(pattern: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .unicode(true)
        .size_limit(512 * 1024)
        .dfa_size_limit(1024 * 1024)
        .build()
}

/// 验证整个候选，不能把超长编号或相邻字母数字截成一个新的验证码。
fn complete_candidate(text: &str, start: usize, end: usize) -> bool {
    let code = &text[start..end];
    let ascii_code = (4..=8).contains(&code.len())
        && code.chars().all(|c| c.is_ascii_alphanumeric())
        && code.chars().any(|c| c.is_ascii_digit());
    let attached = |ch: char| ch.is_ascii_alphanumeric() || ch == '_' || ch.is_numeric();
    ascii_code
        && !text[..start].chars().next_back().is_some_and(attached)
        && !text[end..].chars().next().is_some_and(attached)
}

/// 展示名称只用于提取邮箱，不作为地址匹配的一部分。
fn normalized_address(sender: &str) -> Option<String> {
    let sender = sender.trim();
    let address = if let Some(start) = sender.find('<') {
        let end = sender[start + 1..].find('>')? + start + 1;
        if !sender[end + 1..].trim().is_empty() || sender[..start].contains('>') {
            return None;
        }
        &sender[start + 1..end]
    } else {
        sender
    };
    let address = address.trim();
    if address.matches('@').count() != 1
        || address.starts_with('@')
        || address.ends_with('@')
        || address
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';'))
    {
        return None;
    }
    Some(address.to_ascii_lowercase())
}

fn sender_is_known(sender: Option<&str>, config: &AppConfig) -> bool {
    let Some(address) = sender.and_then(normalized_address) else {
        return false;
    };
    config
        .known_2fa_senders
        .iter()
        .any(|known| normalized_address(known).as_deref() == Some(address.as_str()))
}

/// 仅提取可见文本；不能把 HTML 样式、脚本中的数字当作验证码依据。
fn visible_text(text: &str) -> String {
    static HIDDEN: OnceLock<Regex> = OnceLock::new();
    static TAGS: OnceLock<Regex> = OnceLock::new();
    static ENTITIES: OnceLock<Regex> = OnceLock::new();
    let hidden = HIDDEN.get_or_init(|| {
        Regex::new(r"(?is)<script\b[^>]*>.*?(?:</script\s*>|$)|<style\b[^>]*>.*?(?:</style\s*>|$)")
            .expect("静态 HTML 隐藏内容正则有效")
    });
    let tags = TAGS.get_or_init(|| Regex::new(r"(?s)<[^>]*>").expect("静态 HTML 标签正则有效"));
    let entities = ENTITIES.get_or_init(|| {
        Regex::new(r"&#(?:x([0-9a-fA-F]+)|([0-9]+));").expect("静态 HTML 实体正则有效")
    });
    let without_hidden = hidden.replace_all(text, " ");
    let without_tags = tags.replace_all(&without_hidden, |caps: &Captures<'_>| {
        let tag = caps
            .get(0)
            .expect("静态 HTML 标签正则包含完整匹配")
            .as_str();
        let name = tag
            .trim_start_matches('<')
            .trim_start_matches('/')
            .trim_start()
            .split(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        // 行内标签不能把长数字拆成一个看似完整的短验证码。
        if [
            "p", "div", "br", "li", "tr", "td", "section", "h1", "h2", "h3",
        ]
        .contains(&name.as_str())
        {
            " "
        } else {
            ""
        }
    });
    let decoded = entities.replace_all(&without_tags, |caps: &Captures<'_>| {
        let value = if let Some(hex) = caps.get(1) {
            u32::from_str_radix(hex.as_str(), 16).ok()
        } else {
            caps.get(2)
                .and_then(|decimal| decimal.as_str().parse::<u32>().ok())
        };
        value
            .and_then(char::from_u32)
            .map(|c| c.to_string())
            .unwrap_or_else(|| " ".into())
    });
    decoded
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn negated_anchor(text: &str, start: usize) -> bool {
    // 保留原文本的字符边界，避免 Unicode 大小写转换改变字节偏移。
    let prefix: String = text[..start]
        .chars()
        .rev()
        .take(24)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let prefix = prefix.to_lowercase();
    let prefix = prefix.trim_end();
    ["not a", "not your", "isn't a", "不是", "并非"]
        .iter()
        .any(|s| prefix.ends_with(s))
}

fn semantic_anchors(text: &str, config: &AppConfig) -> Vec<Anchor> {
    let mut anchors = Vec::new();
    for keyword in &config.verification_keywords {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            continue;
        }
        let Ok(regex) = case_insensitive_regex(&regex::escape(keyword)) else {
            continue;
        };
        for found in regex.find_iter(text) {
            // 英文关键词不能命中 STOP、codeword 等单词的子串。
            let ascii_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
            if keyword.chars().next().is_some_and(ascii_word)
                && text[..found.start()]
                    .chars()
                    .next_back()
                    .is_some_and(ascii_word)
            {
                continue;
            }
            if keyword.chars().next_back().is_some_and(ascii_word)
                && text[found.end()..].chars().next().is_some_and(ascii_word)
            {
                continue;
            }
            if !negated_anchor(text, found.start()) {
                anchors.push(Anchor {
                    start: found.start(),
                    end: found.end(),
                    strong: true,
                });
            }
        }
    }
    // 兼容旧 HTML 正常用例 Your code，但这种宽泛提示只允许展示。
    let generic =
        case_insensitive_regex(r"\b(?:your|the)\s+(?:code|pin)\b").expect("静态验证码提示正则有效");
    for found in generic.find_iter(text) {
        if !negated_anchor(text, found.start()) {
            anchors.push(Anchor {
                start: found.start(),
                end: found.end(),
                strong: false,
            });
        }
    }
    anchors
}

fn add_candidate(candidates: &mut BTreeMap<String, VerificationCode>, candidate: VerificationCode) {
    match candidates.get_mut(&candidate.code) {
        Some(existing) if candidate.confidence > existing.confidence => *existing = candidate,
        Some(existing) => existing.auto_fill_eligible |= candidate.auto_fill_eligible,
        None => {
            candidates.insert(candidate.code.clone(), candidate);
        }
    }
}

/// 先验证验证码语义，再确认完整候选；地址和排版都不能单独构成证据。
pub struct CodeExtractor;

impl CodeExtractor {
    pub fn new() -> Self {
        Self
    }

    pub fn extract(
        &self,
        text: &str,
        sender: Option<&str>,
        config: &AppConfig,
    ) -> Option<VerificationCode> {
        // 限制不可信正文的处理成本；超限不截断正文，以免制造假的完整候选。
        if text.trim().is_empty() || text.len() > 2 * 1024 * 1024 {
            return None;
        }
        let text = visible_text(text);
        let anchors = semantic_anchors(&text, config);
        let mut templates = BTreeMap::new();
        for (index, pattern) in config.verification_patterns.iter().enumerate() {
            let regex = match case_insensitive_regex(pattern) {
                Ok(regex) => regex,
                Err(_) => {
                    log::warn!("验证码模板 #{} 无法编译", index + 1);
                    continue;
                }
            };
            for captures in regex.captures_iter(&text).take(32) {
                let (Some(full), Some(code)) = (captures.get(0), captures.get(1)) else {
                    continue;
                };
                if !complete_candidate(&text, code.start(), code.end()) {
                    continue;
                }
                let related: Vec<_> = anchors
                    .iter()
                    .filter(|a| a.start < full.end() && a.end > full.start())
                    .collect();
                if related.is_empty() {
                    continue;
                }
                let strong = related.iter().any(|a| a.strong);
                add_candidate(
                    &mut templates,
                    VerificationCode {
                        code: code.as_str().to_owned(),
                        source: "明确模板".into(),
                        confidence: if strong { 0.95 } else { 0.75 },
                        auto_fill_eligible: strong,
                    },
                );
            }
        }

        // 白名单必须准确匹配，而且仍然要求 use/enter CODE to sign in 的登录语义。
        if sender_is_known(sender, config) {
            let login = case_insensitive_regex(
                r"\b(?:use|enter)\s+([A-Za-z0-9]{4,8})\s+(?:to|for)\s+(?:sign[- ]?in|log[- ]?in)\b",
            )
            .expect("静态登录验证码模板有效");
            for captures in login.captures_iter(&text).take(32) {
                let code = captures.get(1).expect("静态模板包含验证码捕获组");
                if complete_candidate(&text, code.start(), code.end()) {
                    add_candidate(
                        &mut templates,
                        VerificationCode {
                            code: code.as_str().to_owned(),
                            source: "登录语义 + 已知发件人".into(),
                            confidence: 0.90,
                            auto_fill_eligible: true,
                        },
                    );
                }
            }
        }
        if !templates.is_empty() {
            if templates.len() != 1 {
                log::debug!("消息包含不同验证码模板候选，跳过自动选择");
                return None;
            }
            return templates.into_values().next();
        }

        // 没有明确模板时，只收集同一短语内、关键词之后的唯一完整数字候选。
        // 不再用前后80字节“最近数字”抢占工单号，也不授权自动输入。
        let digits = Regex::new(r"[0-9]+").expect("静态数字正则有效");
        let mut contextual = BTreeMap::new();
        for anchor in anchors {
            let suffix = &text[anchor.end..];
            let clause_end = suffix
                .find(['\n', '.', '。', ';', '；', '!', '！', '?', '？'])
                .map(|offset| anchor.end + offset)
                .unwrap_or(text.len());
            let end = text[anchor.end..clause_end]
                .char_indices()
                .nth(80)
                .map(|(offset, _)| anchor.end + offset)
                .unwrap_or(clause_end);
            for found in digits.find_iter(&text[anchor.end..end]) {
                let start = anchor.end + found.start();
                let finish = anchor.end + found.end();
                if complete_candidate(&text, start, finish) {
                    add_candidate(
                        &mut contextual,
                        VerificationCode {
                            code: found.as_str().into(),
                            source: "验证码语义近邻（仅展示）".into(),
                            confidence: 0.75,
                            auto_fill_eligible: false,
                        },
                    );
                }
            }
        }
        if contextual.len() == 1 {
            contextual.into_values().next()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AppConfig {
        AppConfig::default()
    }

    #[test]
    fn test_chinese_template() {
        let extractor = CodeExtractor::new();
        let config = test_config();

        // 中文验证码
        let result = extractor.extract("您的验证码是 384756，请在5分钟内使用", None, &config);
        assert_eq!(result.unwrap().code, "384756");

        let result = extractor.extract("【淘宝】验证码：192837", None, &config);
        assert_eq!(result.unwrap().code, "192837");

        let result = extractor.extract("动态密码为 4829，请勿泄露", None, &config);
        assert_eq!(result.unwrap().code, "4829");
    }

    #[test]
    fn test_english_template() {
        let extractor = CodeExtractor::new();
        let config = test_config();

        let result = extractor.extract("Your verification code is 847291", None, &config);
        assert_eq!(result.unwrap().code, "847291");

        let result = extractor.extract("Your OTP is 293847", None, &config);
        assert_eq!(result.unwrap().code, "293847");

        let result = extractor.extract("Security code: 738291", None, &config);
        assert_eq!(result.unwrap().code, "738291");
    }

    #[test]
    fn test_sender_whitelist() {
        let extractor = CodeExtractor::new();
        let config = test_config();

        let result = extractor.extract(
            "Please use 482917 to sign in.",
            Some("account-security-noreply@accountprotection.microsoft.com"),
            &config,
        );
        assert_eq!(result.unwrap().code, "482917");
    }

    #[test]
    fn test_keyword_proximity() {
        let extractor = CodeExtractor::new();
        let config = test_config();

        let result = extractor.extract(
            "Welcome! Your login code for the service is 918273. It expires in 10 minutes.",
            None,
            &config,
        );
        assert_eq!(result.unwrap().code, "918273");
    }

    #[test]
    fn test_no_false_positive() {
        let extractor = CodeExtractor::new();
        let config = test_config();

        // 普通消息，不应提取出验证码
        let result = extractor.extract("今天天气真好，我们下午3点见面吧", None, &config);
        assert!(result.is_none());
    }

    #[test]
    fn test_html_structure() {
        let extractor = CodeExtractor::new();
        let config = test_config();

        let html = r#"<div><p>Your code:</p><strong>847291</strong></div>"#;
        let result = extractor.extract(html, None, &config);
        assert_eq!(result.unwrap().code, "847291");
    }

    #[test]
    fn exact_sender_matching_rejects_lookalikes_and_address_lists() {
        let config = test_config();
        assert!(sender_is_known(
            Some("Microsoft <NOREPLY@MICROSOFT.COM>"),
            &config
        ));
        for sender in [
            "noreply@microsoft.com.evil.invalid",
            "prefix-noreply@microsoft.com",
            "\"noreply@microsoft.com\" <attacker@evil.invalid>",
            "noreply@microsoft.com,attacker@evil.invalid",
            "noreply@microsoft.com <attacker@evil.invalid>",
            "Microsoft <noreply@microsoft.com> attacker@evil.invalid",
        ] {
            assert!(!sender_is_known(Some(sender), &config), "不能信任冒名地址");
        }
    }

    #[test]
    fn unicode_case_conversion_does_not_shift_keyword_offsets() {
        let config = test_config();
        let result = CodeExtractor::new()
            .extract("İstanbul 提醒：VERIFICATION CODE: 938271", None, &config)
            .unwrap();
        assert_eq!(result.code, "938271");
        assert!(result.auto_fill_eligible);
    }

    #[test]
    fn non_ascii_and_attached_digits_are_not_codes() {
        let config = test_config();
        for text in [
            "验证码：１２３４５６",
            "验证码：123456９",
            "验证码：123456789",
            "Your verification code is abc123456",
            "Your verification code is 123456_xyz",
        ] {
            assert!(CodeExtractor::new().extract(text, None, &config).is_none());
        }
    }

    #[test]
    fn different_explicit_codes_are_ambiguous() {
        let result = CodeExtractor::new().extract(
            "旧验证码：111111。新验证码：222222。",
            None,
            &test_config(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn repeated_identical_code_is_one_candidate() {
        let result = CodeExtractor::new()
            .extract(
                "验证码：123456。Your verification code is 123456.",
                None,
                &test_config(),
            )
            .unwrap();
        assert_eq!(result.code, "123456");
        assert!(result.auto_fill_eligible);
    }

    #[test]
    fn keyword_only_candidate_does_not_authorize_typing() {
        let result = CodeExtractor::new()
            .extract(
                "Your login code for the service is 918273. It expires in 10 minutes.",
                None,
                &test_config(),
            )
            .unwrap();
        assert_eq!(result.code, "918273");
        assert!(!result.auto_fill_eligible);
    }

    #[test]
    fn broad_html_hint_does_not_authorize_typing() {
        let result = CodeExtractor::new()
            .extract(
                "<p>Your code:</p><strong>847291</strong>",
                None,
                &test_config(),
            )
            .unwrap();
        assert_eq!(result.code, "847291");
        assert!(!result.auto_fill_eligible);
    }

    #[test]
    fn format_and_expiry_are_not_otp_semantics() {
        let config = test_config();
        for text in [
            "订单编号：123456（5分钟内有效）",
            "<span class=\"otp\">482917</span><p>Order received.</p>",
            "STOP 482917",
        ] {
            assert!(CodeExtractor::new().extract(text, None, &config).is_none());
        }
    }

    #[test]
    fn script_and_style_contents_are_not_visible_otp_evidence() {
        let result = CodeExtractor::new().extract(
            "<script>Your verification code is 123456</script><style>OTP: 654321</style><p>Order received.</p>",
            None,
            &test_config(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn reverse_chinese_and_english_templates_work() {
        let config = test_config();
        for text in ["384756 是你的验证码", "384756 is your verification code"] {
            let result = CodeExtractor::new().extract(text, None, &config).unwrap();
            assert_eq!(result.code, "384756");
            assert!(result.auto_fill_eligible);
        }
    }

    #[test]
    fn nearby_order_before_keyword_is_not_selected() {
        let result = CodeExtractor::new().extract(
            "Ticket 482917. Your login code will arrive shortly.",
            None,
            &test_config(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn multiple_keyword_only_candidates_are_not_selected() {
        let result = CodeExtractor::new().extract(
            "Your login code for the service is either 111111 or 222222.",
            None,
            &test_config(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn negated_verification_phrase_is_not_a_code() {
        let config = test_config();
        for text in [
            "This is not a verification code: 123456",
            "这不是验证码：123456",
        ] {
            assert!(CodeExtractor::new().extract(text, None, &config).is_none());
        }
    }

    #[test]
    fn html_entities_are_decoded_before_candidate_validation() {
        let result = CodeExtractor::new()
            .extract(
                "<p>Your verification code is &#49;&#50;&#51;&#52;&#53;&#54;</p>",
                None,
                &test_config(),
            )
            .unwrap();
        assert_eq!(result.code, "123456");
    }

    #[test]
    fn inline_html_cannot_split_an_oversized_number_into_a_code() {
        let result = CodeExtractor::new().extract(
            "Your verification code is 123456<span>789</span>",
            None,
            &test_config(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn unclosed_hidden_html_does_not_authorize_a_code() {
        for html in [
            "<p>Order received.</p><script>Your verification code is 123456",
            "<p>Order received.</p><style>OTP: 123456",
        ] {
            assert!(CodeExtractor::new()
                .extract(html, None, &test_config())
                .is_none());
        }
    }
}
