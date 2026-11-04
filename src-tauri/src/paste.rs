use crate::clipboard;
use crate::config::{PasteMode, SharedConfig};
use crate::delivery::{message_is_fresh, target_matches_sender, DeliveryDeduplicator};
use crate::extractor::CodeExtractor;
use crate::monitor::IncomingMessage;
use crate::monitor::SourceKind;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};

#[derive(Clone, Serialize)]
pub struct CodeSnapshot {
    pub code: String,
    pub source: String,
    pub strategy: String,
    pub confidence: f32,
    pub sequence: u64,
    pub expires_at: f64,
}
pub type LatestCode = Arc<Mutex<Option<CodeSnapshot>>>;

pub fn now() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

fn source_enabled(cfg: &crate::config::AppConfig, kind: SourceKind) -> bool {
    match kind {
        SourceKind::Imessage => cfg.listen_imessage,
        SourceKind::AppleMail => cfg.listen_apple_mail,
        SourceKind::Outlook => cfg.listen_outlook,
        SourceKind::Notification => cfg.listen_notification,
    }
}

pub fn show_popup(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("code-popup") {
        return window.show().map_err(|e| e.to_string());
    }
    tauri::WebviewWindowBuilder::new(
        app,
        "code-popup",
        tauri::WebviewUrl::App("popup.html".into()),
    )
    .title("AutoCode 验证码")
    .inner_size(320.0, 260.0)
    .resizable(false)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .focused(false)
    .build()
    .map_err(|e| e.to_string())?;
    Ok(())
}

async fn copy_only(code: String) {
    match tokio::task::spawn_blocking(move || clipboard::copy_to_clipboard(&code)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::warn!("复制验证码失败：{e}"),
        Err(e) => log::warn!("复制任务失败：{e}"),
    }
}

pub async fn handle_message(
    msg: IncomingMessage,
    config: &SharedConfig,
    extractor: &CodeExtractor,
    app: &tauri::AppHandle,
    latest: &LatestCode,
    dedup: &mut DeliveryDeduplicator,
) {
    if !message_is_fresh(msg.received_at, now()) {
        return;
    }
    let Ok(cfg) = config.read().map(|c| c.clone()) else {
        return;
    };
    if !source_enabled(&cfg, msg.kind) {
        return;
    }
    let Some(code) = extractor.extract(&msg.text, msg.sender.as_deref(), &cfg) else {
        return;
    };
    if !dedup.accept(
        &msg.id,
        &code.code,
        msg.sender.as_deref(),
        &msg.text,
        msg.received_at,
        now(),
    ) {
        return;
    }

    let snapshot = {
        let Ok(mut state) = latest.lock() else {
            return;
        };
        let sequence = state.as_ref().map_or(1, |s| s.sequence.saturating_add(1));
        let value = CodeSnapshot {
            code: code.code.clone(),
            source: msg.source.clone(),
            strategy: code.source,
            confidence: code.confidence,
            sequence,
            expires_at: msg.received_at + 120.0,
        };
        *state = Some(value.clone());
        value
    };
    let _ = app.emit("verification-code", &snapshot);

    match cfg.paste_mode {
        PasteMode::ClipboardOnly => {
            copy_only(code.code).await;
            return;
        }
        PasteMode::FloatingOnly => {
            if let Err(e) = show_popup(app) {
                log::warn!("显示验证码提示失败：{e}");
            }
            return;
        }
        _ => {}
    }

    // 模板等级不能单独授权输入，只有唯一明确的验证码候选才进入焦点校验。
    if !code.auto_fill_eligible {
        let _ = show_popup(app);
        return;
    }
    let target = match tokio::task::spawn_blocking(clipboard::capture_focus_target).await {
        Ok(Ok(target)) => target,
        _ => {
            let _ = show_popup(app);
            return;
        }
    };
    if cfg.paste_mode == PasteMode::Smart {
        if target
            .bundle_id
            .as_ref()
            .is_some_and(|id| cfg.native_autofill_apps.contains(id))
        {
            copy_only(code.code).await;
            let _ = show_popup(app);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(
            cfg.autofill_detect_delay_ms,
        ))
        .await;
    }
    if !target_matches_sender(msg.sender.as_deref(), target.origin_host.as_deref()) {
        let _ = show_popup(app);
        return;
    }
    // 等待中改变模式、关闭来源或保存设置，取消旧的待执行输入。
    if !message_is_fresh(msg.received_at, now())
        || !config.read().is_ok_and(|c| c.revision == cfg.revision)
    {
        return;
    }
    let value = code.code.clone();
    let config_for_fill = config.clone();
    let kind = msg.kind;
    let revision = cfg.revision;
    let result = tokio::task::spawn_blocking(move || {
        let current = config_for_fill
            .read()
            .map_err(|_| anyhow::anyhow!("配置锁不可用"))?;
        anyhow::ensure!(
            current.revision == revision && source_enabled(&current, kind),
            "设置已变化，取消待执行输入"
        );
        clipboard::fill_verified_target(&target, &value)
    })
    .await;
    if !matches!(result, Ok(Ok(()))) {
        let _ = show_popup(app);
    }
    // 不发送全局 Return。没有可验证的同服务提交目标时，自动回车必须暂停。
}
