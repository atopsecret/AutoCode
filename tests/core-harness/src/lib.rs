// 此测试包直接引用生产代码；只调用合成消息，不读取个人配置或执行系统输入。
#[path = "../../../src-tauri/src/config.rs"]
pub mod config;
#[path = "../../../src-tauri/src/extractor.rs"]
pub mod extractor;
#[path = "../../../src-tauri/src/delivery.rs"]
pub mod delivery;

#[cfg(test)]
mod review_cases {
    use super::{config::AppConfig, extractor::CodeExtractor};

    #[test]
    fn ordinary_html_number_is_not_otp() {
        let result = CodeExtractor::new().extract(
            "<p>订单编号</p><strong>129900</strong><p>商品已发货</p>",
            None,
            &AppConfig::default(),
        );
        assert!(result.is_none(), "普通订单的加粗数字不是验证码：{result:?}");
    }

    #[test]
    fn ordinary_whitelisted_mail_is_not_otp() {
        let result = CodeExtractor::new().extract(
            "Your monthly receipt number is 482917. Thank you for your purchase.",
            Some("noreply@microsoft.com"),
            &AppConfig::default(),
        );
        assert!(
            result.is_none(),
            "可信发件人的普通订单不是验证码：{result:?}"
        );
    }

    #[test]
    fn sender_lookalike_is_not_trusted() {
        let result = CodeExtractor::new().extract(
            "Order 482917 has shipped.",
            Some("noreply@microsoft.com.evil.invalid"),
            &AppConfig::default(),
        );
        assert!(
            result.is_none(),
            "白名单地址后缀冒名不能得到信任：{result:?}"
        );
    }

    #[test]
    fn long_numeric_run_is_not_truncated() {
        let result = CodeExtractor::new().extract(
            "验证码：123456789，请勿泄露。",
            None,
            &AppConfig::default(),
        );
        assert!(result.is_none(), "超长数字不能截成另一个验证码：{result:?}");
    }

    #[test]
    fn uppercase_template_preserves_correct_candidate() {
        let result = CodeExtractor::new()
            .extract(
                "Ticket 482917. VERIFICATION CODE: 938271",
                None,
                &AppConfig::default(),
            )
            .expect("应识别大写验证码模板");
        assert_eq!(result.code, "938271", "工单编号不能覆盖真正验证码");
    }
}
