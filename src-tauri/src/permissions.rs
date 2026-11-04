use crate::monitor::SourceStatus;
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct PermissionStatus {
    /// 来源是否报告读取被拒绝；不是用 Messages 文件存在性冒充全局授权状态。
    pub full_disk_access: bool,
    pub accessibility: bool,
    pub sources: Vec<SourceStatus>,
}

pub fn check_all(sources: Vec<SourceStatus>) -> PermissionStatus {
    let denied = sources
        .iter()
        .filter_map(|s| s.detail.as_ref())
        .any(|detail| {
            let detail = detail.to_lowercase();
            detail.contains("permission denied")
                || detail.contains("operation not permitted")
                || detail.contains("权限被拒绝")
                || detail.contains("拒绝访问")
        });
    PermissionStatus {
        full_disk_access: !denied,
        accessibility: check_accessibility(),
        sources,
    }
}

#[cfg(target_os = "macos")]
fn check_accessibility() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    unsafe { AXIsProcessTrusted() }
}
#[cfg(not(target_os = "macos"))]
fn check_accessibility() -> bool {
    false
}

pub fn open_full_disk_access_settings() -> Result<()> {
    std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
        .spawn()?;
    Ok(())
}
pub fn open_accessibility_settings() -> Result<()> {
    std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn()?;
    Ok(())
}
