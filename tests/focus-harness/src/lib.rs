// 编译并链接实际 macOS AX/CF 封装；测试不调用任何系统输入、焦点读取或剪贴板。
#[path = "../../../src-tauri/src/delivery.rs"]
pub mod delivery;
#[path = "../../../src-tauri/src/clipboard.rs"]
pub mod clipboard;

#[test]
fn focus_target_can_be_moved_into_blocking_task() {
    fn assert_send<T: Send>(_: fn() -> anyhow::Result<T>) {}
    assert_send(clipboard::capture_focus_target);
}
