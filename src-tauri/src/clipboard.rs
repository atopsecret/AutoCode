use anyhow::{Context, Result};
use std::process::{Command, Stdio};

pub fn copy_to_clipboard(text: &str) -> Result<()> {
    anyhow::ensure!(
        !text.is_empty() && text.len() <= 32,
        "复制内容超过验证码长度限制"
    );
    let mut child = Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .context("启动 pbcopy 失败")?;
    let mut stdin = child.stdin.take().context("无法打开 pbcopy 标准输入")?;
    use std::io::Write;
    if let Err(error) = stdin.write_all(text.as_bytes()) {
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.into());
    }
    drop(stdin);
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => anyhow::bail!("pbcopy 未成功完成"),
            Ok(None) if started.elapsed() < std::time::Duration::from_secs(3) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                match result {
                    Err(e) => return Err(e.into()),
                    _ => anyhow::bail!("pbcopy 执行超时"),
                }
            }
        }
    }
}

/// 写入保留的辅助功能目标，不向全局键盘发送字符或回车。
#[cfg(target_os = "macos")]
mod accessibility {
    use super::*;
    use crate::delivery::TargetMetadata;
    use std::ffi::{c_void, CStr};
    use std::ptr;

    type Ref = *const c_void;
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateSystemWide() -> Ref;
        fn AXUIElementGetTypeID() -> usize;
        fn AXUIElementCopyAttributeValue(element: Ref, attribute: Ref, value: *mut Ref) -> i32;
        fn AXUIElementIsAttributeSettable(element: Ref, attribute: Ref, settable: *mut u8) -> i32;
        fn AXUIElementSetAttributeValue(element: Ref, attribute: Ref, value: Ref) -> i32;
        fn AXUIElementGetPid(element: Ref, pid: *mut i32) -> i32;
        fn AXUIElementSetMessagingTimeout(element: Ref, timeout: f32) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(value: Ref);
        fn CFEqual(a: Ref, b: Ref) -> u8;
        fn CFGetTypeID(value: Ref) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFStringCreateWithBytes(
            allocator: Ref,
            bytes: *const u8,
            len: isize,
            encoding: u32,
            external: u8,
        ) -> Ref;
        fn CFStringGetCString(value: Ref, buffer: *mut i8, len: isize, encoding: u32) -> u8;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(value: Ref) -> u8;
        fn CFURLGetTypeID() -> usize;
        fn CFURLCreateWithString(allocator: Ref, string: Ref, base: Ref) -> Ref;
        fn CFURLCopyHostName(url: Ref) -> Ref;
        fn CFURLCopyScheme(url: Ref) -> Ref;
    }
    extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
    }
    const UTF8: u32 = 0x08000100;

    struct Owned(Ref);
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) }
            }
        }
    }
    // AX 引用持有独立 CF 生命周期，调用在串行的工作任务中执行。
    unsafe impl Send for Owned {}

    fn string(s: &str) -> Result<Owned> {
        let value =
            unsafe { CFStringCreateWithBytes(ptr::null(), s.as_ptr(), s.len() as isize, UTF8, 0) };
        if value.is_null() {
            anyhow::bail!("创建辅助功能字符串失败");
        }
        Ok(Owned(value))
    }

    fn attribute(element: Ref, name: &str) -> Result<Owned> {
        let name = string(name)?;
        let mut result = ptr::null();
        let error = unsafe { AXUIElementCopyAttributeValue(element, name.0, &mut result) };
        if error != 0 || result.is_null() {
            anyhow::bail!("无法取得辅助功能属性，错误 {error}");
        }
        Ok(Owned(result))
    }

    fn text(element: Ref, name: &str) -> Option<String> {
        let value = attribute(element, name).ok()?;
        string_value(value.0)
    }

    fn string_value(value: Ref) -> Option<String> {
        if unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
            return None;
        }
        let mut bytes = vec![0_i8; 4096];
        if unsafe { CFStringGetCString(value, bytes.as_mut_ptr(), bytes.len() as isize, UTF8) } == 0
        {
            return None;
        }
        Some(
            unsafe { CStr::from_ptr(bytes.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn boolean(element: Ref, name: &str) -> bool {
        attribute(element, name).ok().is_some_and(|v| unsafe {
            CFGetTypeID(v.0) == CFBooleanGetTypeID() && CFBooleanGetValue(v.0) != 0
        })
    }

    fn settable(element: Ref, name: &str) -> bool {
        let Ok(name) = string(name) else {
            return false;
        };
        let mut value = 0;
        unsafe { AXUIElementIsAttributeSettable(element, name.0, &mut value) == 0 && value != 0 }
    }

    pub struct FocusTarget {
        element: Owned,
        pub metadata: TargetMetadata,
        pub bundle_id: Option<String>,
        pub origin_host: Option<String>,
    }

    fn origin_host(element: Ref) -> Option<String> {
        let mut current = attribute(element, "AXParent").ok()?;
        for _ in 0..16 {
            unsafe {
                AXUIElementSetMessagingTimeout(current.0, 0.1);
            }
            if text(current.0, "AXRole").as_deref() == Some("AXWebArea") {
                let raw = attribute(current.0, "AXURL").ok()?;
                let parsed;
                let url = if unsafe { CFGetTypeID(raw.0) == CFURLGetTypeID() } {
                    raw.0
                } else if unsafe { CFGetTypeID(raw.0) == CFStringGetTypeID() } {
                    parsed =
                        Owned(unsafe { CFURLCreateWithString(ptr::null(), raw.0, ptr::null()) });
                    if parsed.0.is_null() {
                        return None;
                    }
                    parsed.0
                } else {
                    return None;
                };
                let scheme = Owned(unsafe { CFURLCopyScheme(url) });
                if scheme.0.is_null() || string_value(scheme.0).as_deref() != Some("https") {
                    return None;
                }
                let host = Owned(unsafe { CFURLCopyHostName(url) });
                if host.0.is_null() {
                    return None;
                }
                return string_value(host.0);
            }
            current = attribute(current.0, "AXParent").ok()?;
            if unsafe { CFGetTypeID(current.0) != AXUIElementGetTypeID() } {
                return None;
            }
        }
        None
    }

    fn bundle_id(pid: i32) -> Option<String> {
        let mut bytes = vec![0_u8; 4096];
        if unsafe { proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) } <= 0 {
            return None;
        }
        let path = String::from_utf8(bytes.split(|b| *b == 0).next()?.to_vec()).ok()?;
        let exe = std::path::Path::new(&path);
        let contents = exe
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "Contents"))?;
        let plist = plist::Value::from_file(contents.join("Info.plist")).ok()?;
        Some(
            plist
                .as_dictionary()?
                .get("CFBundleIdentifier")?
                .as_string()?
                .to_owned(),
        )
    }

    pub fn capture_focus_target() -> Result<FocusTarget> {
        let system = Owned(unsafe { AXUIElementCreateSystemWide() });
        if system.0.is_null() {
            anyhow::bail!("辅助功能不可用");
        }
        unsafe {
            AXUIElementSetMessagingTimeout(system.0, 0.3);
        }
        let element = attribute(system.0, "AXFocusedUIElement")?;
        if unsafe { CFGetTypeID(element.0) != AXUIElementGetTypeID() } {
            anyhow::bail!("当前焦点不是辅助功能元素");
        }
        unsafe {
            AXUIElementSetMessagingTimeout(element.0, 0.3);
        }
        let mut pid = 0;
        if unsafe { AXUIElementGetPid(element.0, &mut pid) } != 0 {
            anyhow::bail!("无法确认目标进程");
        }
        let hint = [
            "AXIdentifier",
            "AXPlaceholderValue",
            "AXTitle",
            "AXDescription",
            "AXHelp",
        ]
        .iter()
        .filter_map(|n| text(element.0, n))
        .collect::<Vec<_>>()
        .join(" ");
        let metadata = TargetMetadata {
            pid,
            hint,
            role: text(element.0, "AXRole").unwrap_or_default(),
            enabled: boolean(element.0, "AXEnabled"),
            editable: settable(element.0, "AXSelectedText"),
        };
        if !metadata.is_verification_field() {
            anyhow::bail!("未确认可编辑的验证码输入框");
        }
        Ok(FocusTarget {
            origin_host: origin_host(element.0),
            element,
            metadata,
            bundle_id: bundle_id(pid),
        })
    }

    fn target_still_focused(target: &FocusTarget) -> bool {
        capture_focus_target().ok().is_some_and(|current| {
            current.metadata.pid == target.metadata.pid
                && current.origin_host == target.origin_host
                && unsafe { CFEqual(current.element.0, target.element.0) != 0 }
        })
    }

    pub fn fill_verified_target(target: &FocusTarget, code: &str) -> Result<()> {
        if !target_still_focused(target) {
            anyhow::bail!("焦点已变化，取消自动填入");
        }
        let attribute = string("AXSelectedText")?;
        let value = string(code)?;
        // 定向修改原元素。核验后即使切换应用，也不会向新窗口发送全局键盘事件。
        let error = unsafe { AXUIElementSetAttributeValue(target.element.0, attribute.0, value.0) };
        if error != 0 {
            anyhow::bail!("验证码输入框拒绝填入，错误 {error}");
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub use accessibility::{capture_focus_target, fill_verified_target};
#[cfg(not(target_os = "macos"))]
pub struct FocusTarget {
    pub bundle_id: Option<String>,
    pub origin_host: Option<String>,
}
#[cfg(not(target_os = "macos"))]
pub fn capture_focus_target() -> Result<FocusTarget> {
    anyhow::bail!("自动填入仅支持 macOS");
}
#[cfg(not(target_os = "macos"))]
pub fn fill_verified_target(_: &FocusTarget, _: &str) -> Result<()> {
    anyhow::bail!("自动填入仅支持 macOS");
}
