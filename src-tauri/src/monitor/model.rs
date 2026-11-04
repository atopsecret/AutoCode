use super::SourceKind;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub kind: SourceKind,
    pub source: String,
    pub text: String,
    pub sender: Option<String>,
    pub id: String,
    /// 原始接收时间，Unix 秒。
    pub received_at: f64,
}

#[derive(Debug)]
pub enum MonitorCommand {
    Reconcile {
        enabled: [bool; 4],
        retry_errors: bool,
    },
    Shutdown,
}
pub type MessageSender = mpsc::Sender<IncomingMessage>;
pub type MessageReceiver = mpsc::Receiver<IncomingMessage>;
pub type CommandSender = mpsc::Sender<MonitorCommand>;
