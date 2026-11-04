mod autostart;
mod clipboard;
mod config;
mod delivery;
mod extractor;
mod monitor;
mod paste;
mod permissions;

use config::{load_shared_config, AppConfig, PasteMode, SharedConfig};
use monitor::{MonitorCommand, SharedStatus, SourceKind, SourceStatus};
use paste::{CodeSnapshot, LatestCode};
use std::sync::{Arc, Mutex, RwLock};
use tauri::{Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

struct MonitorController {
    cmd_tx: monitor::CommandSender,
    status: SharedStatus,
}
struct StartupError(Mutex<Option<String>>);
struct PendingUpdate(tokio::sync::Mutex<Option<tauri_plugin_updater::Update>>);
struct TrayMenuState {
    sources: Vec<(SourceKind, tauri::menu::CheckMenuItem<tauri::Wry>)>,
}

fn desired_sources(cfg: &AppConfig) -> [bool; 4] {
    [
        cfg.listen_imessage,
        cfg.listen_apple_mail,
        cfg.listen_outlook,
        cfg.listen_notification,
    ]
}

fn main_window_only(window: &tauri::WebviewWindow) -> Result<(), String> {
    if window.label() != "main" {
        return Err("此操作只能从设置窗口发起".into());
    }
    Ok(())
}

#[tauri::command]
fn get_config(state: tauri::State<SharedConfig>) -> Result<AppConfig, String> {
    state
        .read()
        .map(|s| s.clone())
        .map_err(|e| format!("读取配置失败：{e}"))
}
#[tauri::command]
fn get_default_config() -> AppConfig {
    AppConfig::default()
}
#[tauri::command]
fn get_version() -> String {
    env!("CARGO_PKG_VERSION").into()
}
#[tauri::command]
fn get_startup_error(state: tauri::State<StartupError>) -> Option<String> {
    state.0.lock().ok()?.clone()
}

fn persist_config(app: &tauri::AppHandle, mut new: AppConfig) -> Result<AppConfig, String> {
    new.validate().map_err(|e| e.to_string())?;
    let state = app.state::<SharedConfig>();
    let monitor = app.state::<MonitorController>();
    let mut old = state.write().map_err(|e| format!("读取配置失败：{e}"))?;
    if new.revision != old.revision {
        return Err("配置已被其他窗口或托盘修改，请刷新后重新保存".into());
    }
    // 提前确认命令队列可接收，防止磁盘保存成功却静默丢掉启停操作。
    let permit = monitor
        .cmd_tx
        .try_reserve()
        .map_err(|e| format!("监控控制器暂不可用：{e}"))?;
    new.revision = old.revision.checked_add(1).ok_or("配置版本超出范围")?;
    let autostart_changed = new.launch_at_login != old.launch_at_login;
    if autostart_changed {
        autostart::set_enabled(new.launch_at_login)
            .map_err(|e| format!("设置开机自启失败：{e}"))?;
    }
    if let Err(e) = new.save() {
        if autostart_changed {
            if let Err(rollback) = autostart::set_enabled(old.launch_at_login) {
                return Err(format!("保存失败：{e}；恢复自启状态也失败：{rollback}"));
            }
        }
        return Err(format!("保存配置失败：{e}"));
    }
    *old = new.clone();
    permit.send(MonitorCommand::Reconcile {
        enabled: desired_sources(&new),
        retry_errors: true,
    });
    drop(old);
    sync_tray(app, &new);
    let _ = app.emit("config-changed", &new);
    if let Ok(mut error) = app.state::<StartupError>().0.lock() {
        *error = None;
    }
    let _ = app.emit("startup-error", Option::<String>::None);
    Ok(new)
}

#[tauri::command]
fn update_config(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    new_config: AppConfig,
) -> Result<AppConfig, String> {
    main_window_only(&window)?;
    persist_config(&app, new_config)
}

#[tauri::command]
fn get_monitor_status(state: tauri::State<MonitorController>) -> Result<Vec<SourceStatus>, String> {
    state
        .status
        .lock()
        .map(|s| s.values().cloned().collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn check_permissions(
    state: tauri::State<SharedConfig>,
    monitor: tauri::State<MonitorController>,
) -> Result<permissions::PermissionStatus, String> {
    let cfg = state.read().map_err(|e| e.to_string())?;
    monitor
        .cmd_tx
        .try_send(MonitorCommand::Reconcile {
            enabled: desired_sources(&cfg),
            retry_errors: true,
        })
        .map_err(|e| format!("恢复监控失败：{e}"))?;
    let statuses = monitor
        .status
        .lock()
        .map_err(|e| e.to_string())?
        .values()
        .cloned()
        .collect();
    Ok(permissions::check_all(statuses))
}
#[tauri::command]
fn open_fda_settings(window: tauri::WebviewWindow) -> Result<(), String> {
    main_window_only(&window)?;
    permissions::open_full_disk_access_settings().map_err(|e| e.to_string())
}
#[tauri::command]
fn open_accessibility_settings(window: tauri::WebviewWindow) -> Result<(), String> {
    main_window_only(&window)?;
    permissions::open_accessibility_settings().map_err(|e| e.to_string())
}

#[tauri::command]
fn get_latest_code(state: tauri::State<LatestCode>) -> Option<CodeSnapshot> {
    state
        .lock()
        .ok()?
        .as_ref()
        .filter(|s| s.expires_at > paste::now())
        .cloned()
}
#[tauri::command]
async fn copy_verified_code(
    state: tauri::State<'_, LatestCode>,
    code: String,
) -> Result<(), String> {
    let allowed = state
        .lock()
        .map_err(|e| e.to_string())?
        .as_ref()
        .is_some_and(|s| s.code == code && s.expires_at > paste::now());
    if !allowed {
        return Err("验证码已过期或已被新验证码替代".into());
    }
    let latest = state.inner().clone();
    tokio::task::spawn_blocking(move || {
        let current = latest
            .lock()
            .map_err(|_| anyhow::anyhow!("验证码状态不可用"))?;
        anyhow::ensure!(
            current
                .as_ref()
                .is_some_and(|s| s.code == code && s.expires_at > paste::now()),
            "验证码已过期或已被替代"
        );
        clipboard::copy_to_clipboard(&code)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}
#[tauri::command]
fn dismiss_popup(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("code-popup") {
        w.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct UpdateMetadata {
    version: String,
    current_version: String,
    notes: Option<String>,
}

fn require_update_key(app: &tauri::AppHandle) -> Result<(), String> {
    let key = app
        .config()
        .plugins
        .0
        .get("updater")
        .and_then(|v| v.get("pubkey"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if key.trim().is_empty() {
        return Err(
            "尚未配置签名更新。请使用已验证的正式安装包；不会下载或安装未签名更新。".into(),
        );
    }
    Ok(())
}

#[tauri::command]
async fn check_update(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    pending: tauri::State<'_, PendingUpdate>,
) -> Result<Option<UpdateMetadata>, String> {
    main_window_only(&window)?;
    let mut cached = pending.0.lock().await;
    *cached = None;
    require_update_key(&app)?;
    let mut update = app
        .updater_builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    if let Some(u) = update.as_mut() {
        if u.signature.trim().is_empty() || u.download_url.scheme() != "https" {
            return Err("更新缺少签名或没有使用 HTTPS".into());
        }
        u.timeout = Some(std::time::Duration::from_secs(60));
    }
    let metadata = update.as_ref().map(|u| UpdateMetadata {
        version: u.version.clone(),
        current_version: u.current_version.clone(),
        notes: u.body.clone(),
    });
    *cached = update;
    Ok(metadata)
}

#[tauri::command]
async fn install_update(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    pending: tauri::State<'_, PendingUpdate>,
) -> Result<(), String> {
    main_window_only(&window)?;
    require_update_key(&app)?;
    let mut cached = pending.0.lock().await;
    let update = cached.take().ok_or("没有已检查的更新，请先检查更新")?;
    // 缓存的是已确认版本/URL/签名，不根据安装命令中的用户字符串切换下载目标。
    // 官方 download_and_install 先验公钥签名再安装；不允许关闭验证。
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    drop(cached);
    app.restart();
}

fn sync_tray(app: &tauri::AppHandle, cfg: &AppConfig) {
    if let Some(tray) = app.try_state::<TrayMenuState>() {
        let desired = desired_sources(cfg);
        for ((_, item), enabled) in tray.sources.iter().zip(desired) {
            if let Err(e) = item.set_checked(enabled) {
                log::warn!("刷新托盘失败：{e}");
            }
        }
    }
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn toggle_source(app: &tauri::AppHandle, kind: SourceKind) {
    let state = app.state::<SharedConfig>();
    let Ok(mut cfg) = state.read().map(|s| s.clone()) else {
        return;
    };
    match kind {
        SourceKind::Imessage => cfg.listen_imessage = !cfg.listen_imessage,
        SourceKind::AppleMail => cfg.listen_apple_mail = !cfg.listen_apple_mail,
        SourceKind::Outlook => cfg.listen_outlook = !cfg.listen_outlook,
        SourceKind::Notification => cfg.listen_notification = !cfg.listen_notification,
    }
    if let Err(e) = persist_config(app, cfg) {
        let _ = app.emit("app-error", &e);
        show_main_window(app);
        if let Ok(cfg) = state.read() {
            sync_tray(app, &cfg);
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();
    let (shared, startup_error) = match load_shared_config() {
        Ok(config) => (config, None),
        Err(e) => {
            let detail = format!("配置加载失败，所有监控已暂停，原配置未被覆盖：{e:#}");
            let mut cfg = AppConfig::default();
            cfg.listen_imessage = false;
            cfg.listen_apple_mail = false;
            cfg.listen_outlook = false;
            cfg.listen_notification = false;
            cfg.paste_mode = PasteMode::FloatingOnly;
            (Arc::new(RwLock::new(cfg)), Some(detail))
        }
    };
    if let Ok(mut cfg) = shared.write() {
        cfg.launch_at_login = autostart::is_enabled();
    }
    let latest: LatestCode = Arc::new(Mutex::new(None));
    let latest_for_setup = latest.clone();
    let config_for_setup = shared.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(shared)
        .manage(latest)
        .manage(StartupError(Mutex::new(startup_error)))
        .manage(PendingUpdate(tokio::sync::Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            get_config,
            update_config,
            get_default_config,
            get_version,
            get_startup_error,
            get_monitor_status,
            check_permissions,
            open_fda_settings,
            open_accessibility_settings,
            get_latest_code,
            copy_verified_code,
            dismiss_popup,
            check_update,
            install_update
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let app_handle = app.handle().clone();
            let config = config_for_setup.clone();
            let latest = latest_for_setup.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let Ok(rt) = tokio::runtime::Runtime::new() else {
                    return;
                };
                rt.block_on(async move {
                    let (cmd_tx, mut messages, status, mut status_rx) = monitor::start_monitor();
                    let _ = ready_tx.send(MonitorController {
                        cmd_tx: cmd_tx.clone(),
                        status,
                    });
                    let event_app = app_handle.clone();
                    tokio::spawn(async move {
                        while let Some(status) = status_rx.recv().await {
                            let _ = event_app.emit("monitor-status", status);
                        }
                    });
                    if let Ok(cfg) = config.read() {
                        let _ = cmd_tx.try_send(MonitorCommand::Reconcile {
                            enabled: desired_sources(&cfg),
                            retry_errors: false,
                        });
                    }
                    let extractor = extractor::CodeExtractor::new();
                    let mut dedup = delivery::DeliveryDeduplicator::default();
                    while let Some(msg) = messages.recv().await {
                        paste::handle_message(
                            msg,
                            &config,
                            &extractor,
                            &app_handle,
                            &latest,
                            &mut dedup,
                        )
                        .await;
                    }
                });
            });
            let controller = ready_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .map_err(|e| format!("监控系统初始化失败：{e}"))?;
            app.manage(controller);
            setup_tray(app)?;
            Ok(())
        })
        .on_window_event(|w, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = w.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("AutoCode 启动失败");
}

fn setup_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    use tauri::{
        menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
        tray::TrayIconBuilder,
    };
    let state = app.state::<SharedConfig>();
    let cfg = state.read().map_err(|e| e.to_string())?;
    let source_items = [
        (SourceKind::Imessage, "listen_imessage", "监听 iMessage"),
        (
            SourceKind::AppleMail,
            "listen_apple_mail",
            "监听 Apple Mail",
        ),
        (SourceKind::Outlook, "listen_outlook", "监听 Spotlight 邮件"),
        (
            SourceKind::Notification,
            "listen_notification",
            "监听邮件通知",
        ),
    ];
    let menu = Menu::new(app)?;
    let mut sources = Vec::new();
    for ((kind, id, label), enabled) in source_items.into_iter().zip(desired_sources(&cfg)) {
        let item = CheckMenuItem::with_id(app, id, label, true, enabled, None::<&str>)?;
        menu.append(&item)?;
        sources.push((kind, item));
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "settings",
        "设置…",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "about",
        format!("关于 AutoCode v{}", env!("CARGO_PKG_VERSION")),
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?)?;
    drop(cfg);
    app.manage(TrayMenuState { sources });
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray-icon.png"))?;
    TrayIconBuilder::new()
        .icon(icon)
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip("AutoCode - 验证码提取")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "quit" => {
                let _ = app
                    .state::<MonitorController>()
                    .cmd_tx
                    .try_send(MonitorCommand::Shutdown);
                app.exit(0);
            }
            "settings" => {
                show_main_window(app);
                let _ = app.emit("navigate", "settings");
            }
            "about" => show_main_window(app),
            "listen_imessage" => toggle_source(app, SourceKind::Imessage),
            "listen_apple_mail" => toggle_source(app, SourceKind::AppleMail),
            "listen_outlook" => toggle_source(app, SourceKind::Outlook),
            "listen_notification" => toggle_source(app, SourceKind::Notification),
            _ => {}
        })
        .build(app)?;
    Ok(())
}
