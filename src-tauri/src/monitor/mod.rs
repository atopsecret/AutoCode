use std::sync::Arc;
use tokio::sync::mpsc;

pub mod apple_mail;
pub mod imessage;
pub mod mail_content;
pub mod model;
pub mod notification;
pub mod outlook;
pub mod state;
pub mod supervisor;
pub use model::{CommandSender, IncomingMessage, MessageReceiver, MessageSender, MonitorCommand};
pub use state::{SharedStatus, SourceKind, SourceReporter, SourceStatus};

pub fn start_monitor() -> (
    CommandSender,
    MessageReceiver,
    SharedStatus,
    mpsc::UnboundedReceiver<SourceStatus>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (msg_tx, msg_rx) = mpsc::channel(64);
    let (events, status_rx) = mpsc::unbounded_channel();
    let status = state::new_status();
    let factory: supervisor::SourceFactory = Arc::new(|kind, tx, reporter| {
        Box::pin(async move {
            match kind {
                SourceKind::Imessage => imessage::monitor(tx, reporter).await,
                SourceKind::AppleMail => apple_mail::monitor(tx, reporter).await,
                SourceKind::Outlook => outlook::monitor(tx, reporter).await,
                SourceKind::Notification => notification::monitor(tx, reporter).await,
            }
        })
    });
    let mut actor = supervisor::MonitorActor::new(cmd_rx, msg_tx, status.clone(), events, factory);
    tokio::spawn(async move {
        actor.run().await;
    });
    (cmd_tx, msg_rx, status, status_rx)
}
