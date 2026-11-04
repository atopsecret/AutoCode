// 直接引用生产来源模型、状态与监督者；单测只使用合成任务和消息。
#![allow(dead_code)]
#[path = "../../../src-tauri/src/monitor/model.rs"]
pub mod model;
pub use model::{IncomingMessage, MessageSender, MonitorCommand};
#[path = "../../../src-tauri/src/monitor/state.rs"]
pub mod state;
pub use state::{SharedStatus, SourceKind, SourceReporter, SourceState, SourceStatus};
#[path = "../../../src-tauri/src/monitor/supervisor.rs"]
pub mod supervisor;
#[path = "../../../src-tauri/src/monitor/mail_content.rs"]
pub mod mail_content;
#[path = "../../../src-tauri/src/monitor/apple_mail.rs"]
pub mod apple_mail;
#[path = "../../../src-tauri/src/monitor/outlook.rs"]
pub mod outlook;

#[test]
fn monitor_futures_are_send_without_starting_them() {
    fn assert_send<T: Send>(_: T) {}
    let (tx, _) = tokio::sync::mpsc::channel(1);
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let status = state::new_status();
    assert_send(apple_mail::monitor(tx.clone(), SourceReporter::new(SourceKind::AppleMail, status.clone(), events.clone())));
    assert_send(outlook::monitor(tx, SourceReporter::new(SourceKind::Outlook, status, events)));
}
