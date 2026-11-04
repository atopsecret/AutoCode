use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Imessage,
    AppleMail,
    Outlook,
    Notification,
}

impl SourceKind {
    pub const ALL: [Self; 4] = [
        Self::Imessage,
        Self::AppleMail,
        Self::Outlook,
        Self::Notification,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    Disabled,
    Starting,
    Running,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceStatus {
    pub source: SourceKind,
    pub state: SourceState,
    pub detail: Option<String>,
}

pub type SharedStatus = Arc<Mutex<BTreeMap<SourceKind, SourceStatus>>>;

pub fn new_status() -> SharedStatus {
    Arc::new(Mutex::new(
        SourceKind::ALL
            .into_iter()
            .map(|source| {
                (
                    source,
                    SourceStatus {
                        source,
                        state: SourceState::Disabled,
                        detail: None,
                    },
                )
            })
            .collect(),
    ))
}

#[derive(Clone)]
pub struct SourceReporter {
    source: SourceKind,
    status: SharedStatus,
    events: mpsc::UnboundedSender<SourceStatus>,
}

impl SourceReporter {
    pub fn new(
        source: SourceKind,
        status: SharedStatus,
        events: mpsc::UnboundedSender<SourceStatus>,
    ) -> Self {
        Self {
            source,
            status,
            events,
        }
    }

    pub fn set(&self, state: SourceState, detail: Option<String>) {
        let snapshot = SourceStatus {
            source: self.source,
            state,
            detail,
        };
        if let Ok(mut statuses) = self.status.lock() {
            if statuses
                .get(&self.source)
                .is_some_and(|s| s.state == snapshot.state && s.detail == snapshot.detail)
            {
                return;
            }
            statuses.insert(self.source, snapshot.clone());
        }
        let _ = self.events.send(snapshot);
    }

    pub fn running(&self) {
        self.set(SourceState::Running, None);
    }
    pub fn error(&self, detail: &str) {
        self.set(SourceState::Error, Some(detail.to_owned()));
    }
    pub fn starting(&self) {
        self.set(SourceState::Starting, None);
    }
    pub fn disabled(&self) {
        self.set(SourceState::Disabled, None);
    }
    pub fn is_error(&self) -> bool {
        self.status
            .lock()
            .ok()
            .and_then(|s| s.get(&self.source).map(|s| s.state == SourceState::Error))
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_tracks_failure_recovery_and_stop_independently() {
        let statuses = new_status();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = SourceReporter::new(SourceKind::Notification, statuses.clone(), tx);
        reporter.starting();
        reporter.error("permission denied");
        reporter.running();
        reporter.disabled();
        let states: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|s| s.state)
            .collect();
        assert_eq!(
            states,
            [
                SourceState::Starting,
                SourceState::Error,
                SourceState::Running,
                SourceState::Disabled
            ]
        );
        assert_eq!(
            statuses.lock().unwrap()[&SourceKind::Imessage].state,
            SourceState::Disabled
        );
    }
}
