use anyhow::{Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tokio::sync::mpsc;

use super::mail_content::{parse_emlx, MailContent};
use super::{IncomingMessage, MessageSender, SourceReporter};

static PROCESSED: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

pub(crate) fn find_mail_dir() -> Result<PathBuf> {
    let home = PathBuf::from(env::var("HOME").context("无法获取 HOME 环境变量")?);
    let base = home.join("Library/Mail");
    let mut versions = fs::read_dir(&base)
        .context("无法读取 Apple Mail 目录；请分别检查目录是否存在与访问权限")?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let version = entry
                .file_name()
                .to_str()?
                .strip_prefix('V')?
                .parse::<u32>()
                .ok()?;
            Some((version, entry.path()))
        })
        .collect::<Vec<_>>();
    versions.sort_by_key(|(version, _)| std::cmp::Reverse(*version));
    versions
        .into_iter()
        .find_map(|(_, path)| path.is_dir().then_some(path))
        .context("未找到 Apple Mail 的 V* 数据目录")
}

fn read_content(path: &Path) -> Result<(MailContent, f64)> {
    let metadata = fs::metadata(path).context("邮件文件尚未可读")?;
    anyhow::ensure!(metadata.len() <= 11 * 1024 * 1024, "emlx 超过文本读取上限");
    let raw = fs::read(path).context("读取 emlx 失败")?;
    let mail = parse_emlx(&raw)?;
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_secs_f64();
    Ok((mail, modified))
}

fn message_id(path: &Path, mail: &MailContent) -> String {
    if let Some(id) = &mail.message_id {
        return format!("mail:{id}");
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    mail.text.hash(&mut hasher);
    mail.sender.hash(&mut hasher);
    format!("apple_mail:{:016x}", hasher.finish())
}

struct PendingFile {
    first_seen: Instant,
    next_attempt: Instant,
    attempts: u32,
}

pub async fn monitor(tx: MessageSender, reporter: SourceReporter) -> Result<()> {
    let mail_dir = tokio::task::spawn_blocking(find_mail_dir)
        .await
        .context("Apple Mail 目录检查任务失败")??;
    // notify 回调不阻塞等待异步接收者；主循环按路径合并突发重复事件。
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<Result<PathBuf, String>>();
    let mut watcher =
        notify::recommended_watcher(move |result: notify::Result<Event>| match result {
            Ok(event) if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) => {
                for path in event.paths {
                    if path
                        .extension()
                        .is_some_and(|extension| extension == "emlx")
                    {
                        let _ = events_tx.send(Ok(path));
                    }
                }
            }
            Err(error) => {
                let _ = events_tx.send(Err(error.to_string()));
            }
            _ => {}
        })
        .context("创建 Apple Mail 文件监控器失败")?;
    watcher
        .watch(&mail_dir, RecursiveMode::Recursive)
        .context("启动 Apple Mail 目录监控失败")?;
    reporter.running();
    let processed = PROCESSED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut pending = HashMap::<PathBuf, PendingFile>::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            event = events_rx.recv() => {
                match event.context("Apple Mail 文件事件通道已关闭")? {
                    Ok(path) => {
                        pending.entry(path).or_insert_with(|| PendingFile { first_seen: Instant::now(), next_attempt: Instant::now() + Duration::from_millis(200), attempts: 0 });
                    }
                    Err(error) => anyhow::bail!("Apple Mail 文件监控错误：{error}"),
                }
            }
            _ = tick.tick() => {
                let ready: Vec<_> = pending.iter().filter(|(_, value)| Instant::now() >= value.next_attempt).map(|(path, _)| path.clone()).collect();
                for path in ready {
                    let read_path = path.clone();
                    let result = tokio::task::spawn_blocking(move || read_content(&read_path)).await.context("Apple Mail 解析任务失败")?;
                    match result {
                        Ok((mail, modified)) => {
                            let id = message_id(&path, &mail);
                            let duplicate = {
                                let mut done = processed.lock().map_err(|_| anyhow::anyhow!("邮件去重锁已损坏"))?;
                                done.retain(|_, seen| seen.elapsed() < Duration::from_secs(600));
                                done.contains_key(&id)
                            };
                            if !duplicate && !mail.text.is_empty() {
                                tx.send(IncomingMessage {
                                    kind: super::SourceKind::AppleMail,
                                    source: "Apple Mail".into(), text: mail.text, sender: mail.sender,
                                    id: id.clone(), received_at: mail.received_at.unwrap_or(modified),
                                }).await.context("Apple Mail 消息通道已关闭")?;
                                processed.lock().map_err(|_| anyhow::anyhow!("邮件去重锁已损坏"))?.insert(id, Instant::now());
                            }
                            pending.remove(&path);
                            reporter.running();
                        }
                        Err(error) => {
                            let entry = pending.get_mut(&path).expect("待处理文件存在");
                            entry.attempts += 1;
                            if entry.first_seen.elapsed() > Duration::from_secs(60) {
                                reporter.error(&format!("邮件在60秒内未完成或不受支持：{error}"));
                                pending.remove(&path);
                            } else {
                                entry.next_attempt = Instant::now() + Duration::from_millis((250u64 << entry.attempts.min(4)).min(4000));
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_change_without_message_id_is_new_revision() {
        let mut mail = MailContent {
            text: "验证码123456".into(),
            sender: None,
            message_id: None,
            received_at: None,
        };
        let first = message_id(Path::new("/synthetic/mail.emlx"), &mail);
        mail.text = "验证码654321".into();
        assert_ne!(first, message_id(Path::new("/synthetic/mail.emlx"), &mail));
    }

    #[test]
    fn message_id_is_shared_between_mail_paths() {
        let mail = MailContent {
            text: "验证码123456".into(),
            sender: None,
            message_id: Some("message@example.invalid".into()),
            received_at: None,
        };
        assert_eq!(
            message_id(Path::new("/synthetic/a.emlx"), &mail),
            message_id(Path::new("/synthetic/b.emlx"), &mail)
        );
    }
}
