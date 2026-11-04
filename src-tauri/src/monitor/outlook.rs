use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, UNIX_EPOCH};
use tokio::time::{self, Duration};

use super::{IncomingMessage, MessageSender, SourceReporter};

/// Spotlight 命中的邮件候选
#[derive(Debug, Clone)]
struct SpotlightHit {
    source: String,
    path: String,
}

/// 邮件客户端 Spotlight 扫描目标
#[derive(Debug, Clone)]
struct SpotlightTarget {
    label: String,
    root: PathBuf,
    strict_outlook_filter: bool,
}

/// 从 Spotlight 元数据解析出的邮件信息
#[derive(Debug)]
struct SpotlightEmail {
    subject: Option<String>,
    content: Option<String>,
    author: Option<String>,
}

const POLL_SECONDS: u64 = 3;
const OUTLOOK_LABEL: &str = "Outlook";
const OUTLOOK_ROOT: &str = "Library/Group Containers/UBF8T346G9.Office/Outlook";

static STARTED_AT: OnceLock<f64> = OnceLock::new();
static PROCESSED: OnceLock<Mutex<HashMap<String, (u64, Instant)>>> = OnceLock::new();

/// 避免 mdfind/mdls 卡住监督任务，同时排空管道防止大正文堵塞子进程。
pub(super) fn timed_output(command: &mut Command) -> Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().context("缺少命令 stdout")?;
    let stderr = child.stderr.take().context("缺少命令 stderr")?;
    let out_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(10 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let err_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take(64 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_reader.join();
            let _ = err_reader.join();
            anyhow::bail!("Spotlight 命令超过5秒，已终止本次子进程");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = out_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stdout 读取任务失败"))??;
    let stderr = err_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stderr 读取任务失败"))??;
    anyhow::ensure!(
        stdout.len() <= 10 * 1024 * 1024,
        "Spotlight 返回内容超过10MiB上限"
    );
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn add_target(
    targets: &mut Vec<SpotlightTarget>,
    dedup: &mut HashSet<PathBuf>,
    label: impl Into<String>,
    root: PathBuf,
    strict_outlook_filter: bool,
) {
    if !root.exists() || !root.is_dir() {
        return;
    }
    if dedup.insert(root.clone()) {
        targets.push(SpotlightTarget {
            label: label.into(),
            root,
            strict_outlook_filter,
        });
    }
}

fn is_mail_like_name(name: &str) -> bool {
    let n = name.to_lowercase();
    let keywords = [
        "mail",
        "outlook",
        "spark",
        "airmail",
        "canary",
        "thunderbird",
        "mailspring",
        "postbox",
        "inbox",
        "message",
    ];
    keywords.iter().any(|kw| n.contains(kw))
}

/// 扫描目录中的 mail-like 子目录，自动纳入 Spotlight 目标
fn discover_mail_like_subdirs(
    base: &Path,
    label_prefix: &str,
    add_data_subdir: bool,
    targets: &mut Vec<SpotlightTarget>,
    dedup: &mut HashSet<PathBuf>,
) {
    let entries = match fs::read_dir(base) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let dir_name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !is_mail_like_name(&dir_name) {
            continue;
        }

        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        if add_data_subdir {
            let data_path = path.join("Data");
            if data_path.exists() && data_path.is_dir() {
                add_target(
                    targets,
                    dedup,
                    format!("{}: {}", label_prefix, dir_name),
                    data_path,
                    false,
                );
            } else {
                add_target(
                    targets,
                    dedup,
                    format!("{}: {}", label_prefix, dir_name),
                    path,
                    false,
                );
            }
        } else {
            add_target(
                targets,
                dedup,
                format!("{}: {}", label_prefix, dir_name),
                path,
                false,
            );
        }
    }
}

/// 自动发现可用的 Spotlight 邮件扫描目录（Outlook 保证兼容）
fn discover_targets() -> Result<Vec<SpotlightTarget>> {
    let home = PathBuf::from(env::var("HOME").context("无法获取 HOME")?);
    let mut targets = Vec::new();
    let mut dedup = HashSet::new();

    // 1) Outlook 官方路径（优先 + 保留旧过滤规则）
    add_target(
        &mut targets,
        &mut dedup,
        OUTLOOK_LABEL,
        home.join(OUTLOOK_ROOT),
        true,
    );

    // 2) 常见邮件客户端路径
    let common_roots = [
        ("Thunderbird", "Library/Thunderbird/Profiles", false),
        (
            "Mailspring",
            "Library/Application Support/Mailspring",
            false,
        ),
        ("MailMate", "Library/Application Support/MailMate", false),
        (
            "Airmail",
            "Library/Containers/it.bloop.airmail2/Data",
            false,
        ),
        (
            "Spark",
            "Library/Containers/com.readdle.smartemail-Mac/Data",
            false,
        ),
        (
            "Canary Mail",
            "Library/Containers/io.canarymail.mac/Data",
            false,
        ),
    ];
    for (label, rel, strict) in common_roots {
        add_target(&mut targets, &mut dedup, label, home.join(rel), strict);
    }

    // 3) 动态发现 mail-like 容器目录（提升通用性）
    discover_mail_like_subdirs(
        &home.join("Library/Containers"),
        "Container",
        true,
        &mut targets,
        &mut dedup,
    );
    discover_mail_like_subdirs(
        &home.join("Library/Group Containers"),
        "GroupContainer",
        false,
        &mut targets,
        &mut dedup,
    );
    discover_mail_like_subdirs(
        &home.join("Library/Application Support"),
        "AppSupport",
        false,
        &mut targets,
        &mut dedup,
    );

    Ok(targets)
}

fn source_name_for_target(target: &SpotlightTarget) -> String {
    if target.label == OUTLOOK_LABEL {
        OUTLOOK_LABEL.to_string()
    } else {
        format!("Spotlight ({})", target.label)
    }
}

fn is_likely_mail_candidate(path: &str, strict_outlook_filter: bool) -> bool {
    if strict_outlook_filter {
        // 兼容旧版 Outlook 过滤规则，保证现有行为稳定
        return path.contains("olk16") || path.contains("olk15") || path.ends_with("Message");
    }

    let p = path.to_lowercase();
    p.ends_with(".emlx")
        || p.ends_with(".eml")
        || p.ends_with(".msg")
        || p.contains("/mail/")
        || p.contains("/messages/")
        || p.contains("outlook")
        || p.contains("thunderbird")
        || p.contains("mailspring")
        || p.contains("canary")
        || p.contains("spark")
        || p.contains("airmail")
}

/// 在指定目标目录中查询最近变更文件
fn query_recent_in_target(
    target: &SpotlightTarget,
    since_seconds: u64,
) -> Result<Vec<SpotlightHit>> {
    let output = timed_output(Command::new("/usr/bin/mdfind").args([
        "-onlyin",
        target.root.to_string_lossy().as_ref(),
        &format!(
            "kMDItemContentModificationDate >= $time.now(-{})",
            since_seconds
        ),
    ]))
    .context("执行 mdfind 失败")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("mdfind 返回错误 [{}]: {}", target.label, stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let hits = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| is_likely_mail_candidate(line, target.strict_outlook_filter))
        .map(|p| SpotlightHit {
            source: source_name_for_target(target),
            path: p.to_string(),
        })
        .collect();

    Ok(hits)
}

/// 汇总多个目标目录的 Spotlight 结果，并按路径去重
fn query_recent_spotlight_emails(
    targets: &[SpotlightTarget],
    since_seconds: u64,
) -> Result<Vec<SpotlightHit>> {
    let mut merged = Vec::new();
    let mut seen_paths = HashSet::new();

    for target in targets {
        for hit in query_recent_in_target(target, since_seconds)? {
            if seen_paths.insert(hit.path.clone()) {
                merged.push(hit);
            }
        }
    }

    Ok(merged)
}

/// 使用 mdls 获取文件的 Spotlight 元数据
fn get_spotlight_metadata(path: &str) -> Result<SpotlightEmail> {
    let output = timed_output(Command::new("/usr/bin/mdls").args([
        "-raw",
        "-nullMarker",
        "[[AUTOCODE_NULL]]",
        "-name",
        "kMDItemSubject",
        "-name",
        "kMDItemTextContent",
        "-name",
        "kMDItemAuthors",
        "-name",
        "kMDItemDisplayName",
        path,
    ]))
    .context("执行 mdls 失败")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("mdls 命令失败: {}", stderr.trim());
    }

    parse_spotlight_metadata(&output.stdout)
}

fn parse_spotlight_metadata(raw: &[u8]) -> Result<SpotlightEmail> {
    // mdls(1) 的 -raw 明确按请求顺序用 NUL 分隔字段；正文中的换行、引号或等号不再被误当字段。
    let mut fields = raw.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields.last().is_some_and(|field| field.is_empty()) {
        fields.pop();
    }
    anyhow::ensure!(fields.len() == 4, "mdls -raw 字段数量不符合预期");
    let mut email = SpotlightEmail {
        subject: None,
        content: None,
        author: None,
    };
    for (key, raw) in [
        "kMDItemSubject",
        "kMDItemTextContent",
        "kMDItemAuthors",
        "kMDItemDisplayName",
    ]
    .into_iter()
    .zip(fields)
    {
        let value = std::str::from_utf8(raw).context("Spotlight 文本不是有效 UTF-8")?;
        if value != "[[AUTOCODE_NULL]]" {
            apply_metadata_value(&mut email, key, value);
        }
    }
    Ok(email)
}

/// 将解析到的值应用到邮件结构体
fn apply_metadata_value(email: &mut SpotlightEmail, key: &str, value: &str) {
    let clean_value = value.trim().to_string();
    if clean_value.is_empty() || clean_value == "(null)" {
        return;
    }

    // 处理数组格式 ("val1", "val2")
    let final_value = if key == "kMDItemAuthors" && clean_value.starts_with('(') {
        clean_value
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(',')
            .next()
            .map(|v| v.trim().trim_matches('"').trim().to_string())
            .unwrap_or_default()
    } else {
        clean_value
    };

    if final_value.is_empty() {
        return;
    }

    match key {
        "kMDItemSubject" => email.subject = Some(final_value),
        "kMDItemTextContent" => email.content = Some(final_value),
        "kMDItemAuthors" => email.author = Some(final_value),
        "kMDItemDisplayName" => {
            if email.subject.is_none() {
                email.subject = Some(final_value);
            }
        }
        _ => {}
    }
}

fn file_signature(path: &str) -> Result<(u64, f64)> {
    let metadata = fs::metadata(path)?;
    let modified = metadata.modified()?.duration_since(UNIX_EPOCH)?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    metadata.len().hash(&mut hash);
    modified.as_nanos().hash(&mut hash);
    Ok((hash.finish(), modified.as_secs_f64()))
}

fn email_text(email: &SpotlightEmail) -> Option<String> {
    // 只有标题而正文仍是 null 时，不提前标记 processed；索引可能尚未完成。
    let content = email
        .content
        .as_ref()
        .filter(|body| !body.trim().is_empty())?;
    Some(
        match email.subject.as_ref().filter(|subject| !subject.is_empty()) {
            Some(subject) => format!("{subject}\n{content}"),
            None => content.clone(),
        },
    )
}

pub async fn monitor(tx: MessageSender, reporter: SourceReporter) -> Result<()> {
    let started_at =
        *STARTED_AT.get_or_init(|| chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
    let processed = PROCESSED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut targets = tokio::task::spawn_blocking(discover_targets)
        .await
        .context("Spotlight 目录发现任务失败")??;
    anyhow::ensure!(
        !targets.is_empty(),
        "未发现可用 Spotlight 邮件目录；监督者稍后重新检查"
    );
    let mut pending = HashMap::<String, (SpotlightHit, Instant)>::new();
    let mut interval = time::interval(Duration::from_secs(POLL_SECONDS));
    let mut tick_count = 0u64;
    loop {
        interval.tick().await;
        tick_count += 1;
        if tick_count % 40 == 0 {
            targets = tokio::task::spawn_blocking(discover_targets)
                .await
                .context("Spotlight 目录更新任务失败")??;
            anyhow::ensure!(!targets.is_empty(), "Spotlight 邮件目录当前不可用");
        }
        let query_targets = targets.clone();
        let hits =
            tokio::task::spawn_blocking(move || query_recent_spotlight_emails(&query_targets, 120))
                .await
                .context("Spotlight 查询任务失败")??;
        for hit in hits {
            // 不读取启动前的历史邮件；监督重启复用同一启动时间。
            let signature_path = hit.path.clone();
            let signature = tokio::task::spawn_blocking(move || file_signature(&signature_path))
                .await
                .context("Spotlight 文件检查任务失败")?;
            if let Ok((signature, modified)) = signature {
                if modified < started_at {
                    continue;
                }
                let duplicate = {
                    let mut done = processed
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Spotlight 去重锁已损坏"))?;
                    done.retain(|_, (_, seen)| seen.elapsed() < Duration::from_secs(600));
                    done.get(&hit.path)
                        .is_some_and(|(old, _)| *old == signature)
                };
                if duplicate {
                    continue;
                }
            }
            pending
                .entry(hit.path.clone())
                .or_insert_with(|| (hit, Instant::now()));
        }
        let candidates: Vec<_> = pending.values().cloned().collect();
        let mut healthy = true;
        for (hit, first_seen) in candidates {
            let candidate_path = hit.path.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<_> {
                let email = get_spotlight_metadata(&candidate_path)?;
                let text = email_text(&email).context("Spotlight 邮件正文尚未建立索引")?;
                let (signature, received_at) = file_signature(&candidate_path)?;
                Ok((email, text, signature, received_at))
            })
            .await
            .context("Spotlight 元数据任务失败")?;
            match result {
                Ok((email, text, signature, received_at)) => {
                    tx.send(IncomingMessage {
                        kind: super::SourceKind::Outlook,
                        source: hit.source,
                        text,
                        sender: email.author,
                        id: format!("spotlight:{}:{signature:016x}", hit.path),
                        received_at,
                    })
                    .await
                    .context("Spotlight 消息通道已关闭")?;
                    processed
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Spotlight 去重锁已损坏"))?
                        .insert(hit.path.clone(), (signature, Instant::now()));
                    pending.remove(&hit.path);
                }
                Err(error) => {
                    healthy = false;
                    reporter.error(&format!("Spotlight 元数据暂不可用：{error}"));
                    if first_seen.elapsed() > Duration::from_secs(600) {
                        pending.remove(&hit.path);
                    }
                    // 失败不写入 processed；即使移出最近查询窗口，也由 pending 继续重试。
                }
            }
        }
        if healthy {
            reporter.running();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_metadata_preserves_multiline_equals_and_quotes() {
        let parsed = parse_spotlight_metadata(b"Verification\0line = one\nVerification code: 123456 \"quoted\"\0(sample@example.invalid)\0display\0").unwrap();
        assert_eq!(
            parsed.content.as_deref(),
            Some("line = one\nVerification code: 123456 \"quoted\"")
        );
        assert_eq!(parsed.author.as_deref(), Some("sample@example.invalid"));
    }

    #[test]
    fn missing_content_remains_retryable() {
        let parsed = parse_spotlight_metadata(
            b"Verification code\0[[AUTOCODE_NULL]]\0[[AUTOCODE_NULL]]\0display",
        )
        .unwrap();
        assert!(email_text(&parsed).is_none());
    }

    #[test]
    fn parentheses_in_body_are_not_treated_as_author_array() {
        let parsed = parse_spotlight_metadata(
            b"subject\0(Verification code: 123456)\0[[AUTOCODE_NULL]]\0display",
        )
        .unwrap();
        assert_eq!(
            parsed.content.as_deref(),
            Some("(Verification code: 123456)")
        );
    }
}
