//! 来源监督者。来源工厂可替换成合成任务，测试不打开真实数据源。
use super::{
    MessageSender, MonitorCommand, SharedStatus, SourceKind, SourceReporter, SourceStatus,
};
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub type SourceFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
pub type SourceFactory =
    Arc<dyn Fn(SourceKind, MessageSender, SourceReporter) -> SourceFuture + Send + Sync>;

struct SourceTask {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}
pub struct MonitorActor {
    cmd_rx: mpsc::Receiver<MonitorCommand>,
    msg_tx: MessageSender,
    tasks: BTreeMap<SourceKind, SourceTask>,
    status: SharedStatus,
    events: mpsc::UnboundedSender<SourceStatus>,
    factory: SourceFactory,
}

impl MonitorActor {
    pub fn new(
        cmd_rx: mpsc::Receiver<MonitorCommand>,
        msg_tx: MessageSender,
        status: SharedStatus,
        events: mpsc::UnboundedSender<SourceStatus>,
        factory: SourceFactory,
    ) -> Self {
        Self {
            cmd_rx,
            msg_tx,
            status,
            events,
            factory,
            tasks: BTreeMap::new(),
        }
    }
    fn reporter(&self, kind: SourceKind) -> SourceReporter {
        SourceReporter::new(kind, self.status.clone(), self.events.clone())
    }
    async fn stop(&mut self, kind: SourceKind) {
        if let Some(task) = self.tasks.remove(&kind) {
            task.cancel.cancel();
            let mut handle = task.handle;
            if tokio::time::timeout(std::time::Duration::from_secs(3), &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                let _ = handle.await;
            }
        }
        self.reporter(kind).disabled();
    }
    fn start(&mut self, kind: SourceKind) {
        let reporter = self.reporter(kind);
        reporter.starting();
        let tx = self.msg_tx.clone();
        let factory = self.factory.clone();
        let cancel = CancellationToken::new();
        let cancelled = cancel.clone();
        let handle = tokio::spawn(async move {
            let mut retry_seconds = 2;
            loop {
                reporter.starting();
                // 子任务 JoinError 捕获 panic；不能让来源假装仍是 running。
                let mut source = tokio::spawn(factory(kind, tx.clone(), reporter.clone()));
                let result = tokio::select! {
                    result = &mut source => result,
                    _ = cancelled.cancelled() => { source.abort(); let _ = source.await; return; },
                };
                let detail = match result {
                    Ok(Ok(())) => "监控任务已退出，将重试".to_owned(),
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(e) => format!("监控任务异常退出：{e}"),
                };
                reporter.error(&detail);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(retry_seconds)) => {},
                    _ = cancelled.cancelled() => return,
                }
                retry_seconds = (retry_seconds * 2).min(30);
            }
        });
        self.tasks.insert(kind, SourceTask { cancel, handle });
    }
    pub async fn run(&mut self) {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                MonitorCommand::Reconcile {
                    enabled,
                    retry_errors,
                } => {
                    for (kind, enabled) in SourceKind::ALL.into_iter().zip(enabled) {
                        if !enabled {
                            self.stop(kind).await;
                            continue;
                        }
                        let dead = self
                            .tasks
                            .get(&kind)
                            .is_some_and(|t| t.handle.is_finished());
                        if dead || (retry_errors && self.reporter(kind).is_error()) {
                            self.stop(kind).await;
                        }
                        if !self.tasks.contains_key(&kind) {
                            self.start(kind);
                        }
                    }
                }
                MonitorCommand::Shutdown => break,
            }
        }
        for kind in SourceKind::ALL {
            self.stop(kind).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::{new_status, SourceState};
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    async fn wait_for(status: &SharedStatus, kind: SourceKind, state: SourceState) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if status.lock().unwrap()[&kind].state == state {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("合成来源未到达预期状态");
    }

    #[tokio::test]
    async fn authorization_recheck_recovers_and_does_not_gate_other_sources() {
        let allowed = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicUsize::new(0));
        let permission = allowed.clone();
        let calls = attempts.clone();
        let factory: SourceFactory = Arc::new(move |kind, _, reporter| {
            let permission = permission.clone();
            let calls = calls.clone();
            Box::pin(async move {
                if kind == SourceKind::Imessage {
                    calls.fetch_add(1, Ordering::SeqCst);
                    anyhow::ensure!(
                        permission.load(Ordering::SeqCst),
                        "synthetic permission denied"
                    );
                }
                reporter.running();
                std::future::pending::<()>().await;
                Ok(())
            })
        });
        let status = new_status();
        let (events, _) = mpsc::unbounded_channel();
        let (tx, rx) = mpsc::channel(8);
        let (messages, _) = mpsc::channel(8);
        let mut actor = MonitorActor::new(rx, messages, status.clone(), events, factory);
        let run = tokio::spawn(async move {
            actor.run().await;
        });
        tx.send(MonitorCommand::Reconcile {
            enabled: [true, true, false, true],
            retry_errors: false,
        })
        .await
        .unwrap();
        wait_for(&status, SourceKind::Imessage, SourceState::Error).await;
        wait_for(&status, SourceKind::AppleMail, SourceState::Running).await;
        wait_for(&status, SourceKind::Notification, SourceState::Running).await;
        allowed.store(true, Ordering::SeqCst);
        tx.send(MonitorCommand::Reconcile {
            enabled: [true, true, false, true],
            retry_errors: true,
        })
        .await
        .unwrap();
        wait_for(&status, SourceKind::Imessage, SourceState::Running).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        tx.send(MonitorCommand::Reconcile {
            enabled: [true, true, false, true],
            retry_errors: true,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "同值保存不重复启动健康来源"
        );
        tx.send(MonitorCommand::Reconcile {
            enabled: [false; 4],
            retry_errors: false,
        })
        .await
        .unwrap();
        wait_for(&status, SourceKind::Imessage, SourceState::Disabled).await;
        tx.send(MonitorCommand::Shutdown).await.unwrap();
        run.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_failed_source_backoff() {
        let factory: SourceFactory =
            Arc::new(|_, _, _| Box::pin(async { Err(anyhow::anyhow!("synthetic failure")) }));
        let status = new_status();
        let (events, _) = mpsc::unbounded_channel();
        let (tx, rx) = mpsc::channel(8);
        let (messages, _) = mpsc::channel(8);
        let mut actor = MonitorActor::new(rx, messages, status.clone(), events, factory);
        let run = tokio::spawn(async move {
            actor.run().await;
        });
        tx.send(MonitorCommand::Reconcile {
            enabled: [false, false, false, true],
            retry_errors: true,
        })
        .await
        .unwrap();
        wait_for(&status, SourceKind::Notification, SourceState::Error).await;
        tx.send(MonitorCommand::Shutdown).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.lock().unwrap()[&SourceKind::Notification].state,
            SourceState::Disabled
        );
    }

    #[tokio::test]
    async fn panic_is_reported_and_automatically_retried() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let factory: SourceFactory = Arc::new(move |_, _, reporter| {
            let count = count.clone();
            Box::pin(async move {
                let call = count.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    panic!("synthetic source panic");
                }
                reporter.running();
                std::future::pending::<()>().await;
                Ok(())
            })
        });
        let status = new_status();
        let (events, _) = mpsc::unbounded_channel();
        let (tx, rx) = mpsc::channel(8);
        let (messages, _) = mpsc::channel(8);
        let mut actor = MonitorActor::new(rx, messages, status.clone(), events, factory);
        let run = tokio::spawn(async move {
            actor.run().await;
        });
        tx.send(MonitorCommand::Reconcile {
            enabled: [false, false, false, true],
            retry_errors: false,
        })
        .await
        .unwrap();
        wait_for(&status, SourceKind::Notification, SourceState::Error).await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if status.lock().unwrap()[&SourceKind::Notification].state == SourceState::Running {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        tx.send(MonitorCommand::Shutdown).await.unwrap();
        run.await.unwrap();
    }
}
