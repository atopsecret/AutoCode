use anyhow::{Context, Result};
use plist::Value;
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::env;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::time;

use super::{IncomingMessage, MessageSender, SourceReporter};

const CF_EPOCH: f64 = 978_307_200.0;
const PAGE_SIZE: i64 = 200;
const OVERLAP_SECONDS: f64 = 120.0;
const DEDUP_SECONDS: u64 = 600;

/// ROWID 捕获延迟入库；时间重叠窗口捕获已有记录的延迟填充。
#[derive(Default)]
struct NotificationState {
    initialized: bool,
    database: PathBuf,
    high_water: i64,
    started_at: f64,
    last_success: f64,
    processed: HashMap<String, Instant>,
}

// 保留监督者重启前的进度；不会写入用户通知数据库。
static STATE: OnceLock<Mutex<NotificationState>> = OnceLock::new();

#[derive(Debug)]
struct NotificationRecord {
    rowid: i64,
    app_identifier: String,
    uuid: Vec<u8>,
    data: Vec<u8>,
    delivered_date: f64,
}

fn database_candidates(home: &Path, darwin_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = vec![home.join("Library/Group Containers/group.com.apple.usernoted/db2/db")];
    if let Some(dir) = darwin_dir {
        paths.push(dir.join("com.apple.notificationcenter/db2/db"));
        paths.push(dir.join("com.apple.notificationcenter/db/db"));
    }
    paths.push(home.join("Library/Application Support/NotificationCenter/db2/db"));
    paths
}

pub(crate) fn get_notification_db_path() -> Result<PathBuf> {
    let home = PathBuf::from(env::var("HOME").context("无法获取 HOME 环境变量")?);
    // getconf 只返回当前用户缓存目录，不读取通知内容。
    let darwin_dir = env::var_os("DARWIN_USER_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            super::outlook::timed_output(Command::new("/usr/bin/getconf").arg("DARWIN_USER_DIR"))
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|value| PathBuf::from(value.trim()))
        });
    for path in database_candidates(&home, darwin_dir.as_deref()) {
        match path.try_exists() {
            Ok(true) => return Ok(path),
            Ok(false) => {}
            Err(error) => return Err(error).context("无法检查通知数据库目录权限"),
        }
    }
    anyhow::bail!("未找到通知数据库；系统通知目录尚未创建或当前账户无访问权限")
}

fn validate_schema(conn: &Connection) -> Result<()> {
    for (table, required) in [
        ("record", &["app_id", "uuid", "data", "delivered_date"][..]),
        ("app", &["app_id", "identifier"][..]),
    ] {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if required
            .iter()
            .any(|name| !columns.iter().any(|column| column == name))
        {
            anyhow::bail!(
                "通知数据库 schema 不受支持：{table} 缺少所需列；不会猜测字段或跳过权限检查"
            );
        }
    }
    Ok(())
}

fn query_page(
    conn: &Connection,
    after: i64,
    high_water: i64,
    since: f64,
) -> Result<Vec<NotificationRecord>> {
    let mut stmt = conn.prepare(
        "SELECT r.ROWID, a.identifier, r.uuid, r.data, r.delivered_date
         FROM record r JOIN app a ON a.app_id = r.app_id
         WHERE r.ROWID > ?1 AND (r.ROWID > ?2 OR r.delivered_date >= ?3)
         ORDER BY r.ROWID ASC LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![after, high_water, since, PAGE_SIZE],
        |row| {
            Ok(NotificationRecord {
                rowid: row.get(0)?,
                app_identifier: row.get(1)?,
                uuid: match row.get_ref(2)? {
                    rusqlite::types::ValueRef::Blob(value)
                    | rusqlite::types::ValueRef::Text(value) => value.to_vec(),
                    rusqlite::types::ValueRef::Null => Vec::new(),
                    other => {
                        return Err(rusqlite::Error::InvalidColumnType(
                            2,
                            "uuid".into(),
                            other.data_type(),
                        ))
                    }
                },
                data: row.get(3)?,
                delivered_date: row.get(4)?,
            })
        },
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("通知记录字段类型与已支持 schema 不一致")
}

fn notification_text(data: &[u8]) -> Result<String> {
    anyhow::ensure!(data.len() <= 1024 * 1024, "通知 plist 超过1MiB解析上限");
    let value = Value::from_reader(Cursor::new(data)).context("通知 data 不是有效 plist")?;
    let req = value
        .as_dictionary()
        .and_then(|root| root.get("req"))
        .and_then(Value::as_dictionary)
        .context("通知 plist 缺少 req 字典")?;
    // 记录可能先有标题后填入正文；此时不能把 UUID 永久标记已处理。
    if req
        .get("body")
        .is_none_or(|body| body.as_string().is_some_and(|text| text.trim().is_empty()))
    {
        return Ok(String::new());
    }
    let mut parts = Vec::new();
    for key in ["titl", "subt", "body"] {
        match req.get(key) {
            Some(Value::String(text)) if !text.trim().is_empty() => {
                parts.push(text.trim().to_owned())
            }
            Some(Value::String(_)) | None => {}
            Some(_) => anyhow::bail!("通知 plist 的 {key} 字段类型不受支持"),
        }
    }
    Ok(parts.join("\n"))
}

fn is_mail_app(app_id: &str) -> bool {
    let id = app_id.to_ascii_lowercase();
    [
        "mail",
        "outlook",
        "spark",
        "canary",
        "thunderbird",
        "mailspring",
        "postbox",
    ]
    .iter()
    .any(|name| id.contains(name))
}

fn record_id(record: &NotificationRecord) -> String {
    if record.uuid.is_empty() {
        format!(
            "notification:{}:row:{}",
            record.app_identifier, record.rowid
        )
    } else {
        let uuid: String = record
            .uuid
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("notification:{}:{uuid}", record.app_identifier)
    }
}

fn cf_now() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0 - CF_EPOCH
}

pub async fn monitor(tx: MessageSender, reporter: SourceReporter) -> Result<()> {
    let (mut conn, path, max_rowid) = tokio::task::spawn_blocking(|| -> Result<_> {
        let path = get_notification_db_path()?;
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("无法只读打开通知数据库；请检查该来源的完全磁盘访问权限")?;
        conn.busy_timeout(Duration::from_secs(1))?;
        validate_schema(&conn)?;
        let max_rowid: i64 =
            conn.query_row("SELECT COALESCE(MAX(ROWID), 0) FROM record", [], |row| {
                row.get(0)
            })?;
        Ok((conn, path, max_rowid))
    })
    .await
    .context("通知数据库初始化任务失败")??;
    let state_mutex = STATE.get_or_init(|| Mutex::new(NotificationState::default()));
    {
        let mut state = state_mutex
            .lock()
            .map_err(|_| anyhow::anyhow!("通知游标锁已损坏"))?;
        if !state.initialized || state.database != path {
            *state = NotificationState {
                initialized: true,
                database: path,
                high_water: max_rowid,
                started_at: cf_now(),
                last_success: cf_now(),
                processed: HashMap::new(),
            };
        } else if max_rowid < state.high_water {
            state.high_water = 0;
            state.processed.clear();
        }
    }
    reporter.running();
    let mut interval = time::interval(Duration::from_secs(2));
    loop {
        interval.tick().await;
        let (high_water, since, started_at) = {
            let mut state = state_mutex
                .lock()
                .map_err(|_| anyhow::anyhow!("通知游标锁已损坏"))?;
            state
                .processed
                .retain(|_, seen| seen.elapsed() < Duration::from_secs(DEDUP_SECONDS));
            (
                state.high_water,
                state.started_at.max(state.last_success - OVERLAP_SECONDS),
                state.started_at,
            )
        };
        let (returned_conn, result) = tokio::task::spawn_blocking(move || {
            let result = (|| -> Result<_> {
                let max: i64 =
                    conn.query_row("SELECT COALESCE(MAX(ROWID),0) FROM record", [], |row| {
                        row.get(0)
                    })?;
                let high_water = if max < high_water { 0 } else { high_water };
                let mut after = 0;
                let mut new_high_water = high_water;
                let mut records = Vec::new();
                // 完整 drain 后才推进高水位。任意分页失败时保持上轮进度。
                loop {
                    let page = query_page(&conn, after, high_water, since)?;
                    if page.is_empty() {
                        break;
                    }
                    after = page.last().expect("非空分页").rowid;
                    new_high_water = new_high_water.max(after);
                    records.extend(page);
                }
                Ok((new_high_water, records))
            })();
            (conn, result)
        })
        .await
        .context("通知数据库查询任务失败")?;
        conn = returned_conn;
        let (new_high_water, mut records) = result?;
        // 正常批次先旧后新，消费者还会按 received_at 拒绝迟到旧验证码。
        records.sort_by(|a, b| {
            a.delivered_date
                .total_cmp(&b.delivered_date)
                .then(a.rowid.cmp(&b.rowid))
        });
        let mut healthy = true;
        for record in records {
            if !is_mail_app(&record.app_identifier) || record.delivered_date < started_at {
                continue;
            }
            let id = record_id(&record);
            let already_processed = state_mutex
                .lock()
                .map_err(|_| anyhow::anyhow!("通知游标锁已损坏"))?
                .processed
                .contains_key(&id);
            if already_processed {
                continue;
            }
            let text = match notification_text(&record.data) {
                Ok(text) if !text.is_empty() => text,
                Ok(_) => continue, // 保留时间重叠窗，等待正文后续填充。
                Err(error) => {
                    // 明确报告解析错误，不记录正文，也不把错误字段当空字符串。
                    reporter.error(&format!("通知记录 {} 解析失败：{error}", record.rowid));
                    healthy = false;
                    continue;
                }
            };
            let source = if record
                .app_identifier
                .to_ascii_lowercase()
                .contains("outlook")
            {
                "Outlook (通知)".to_owned()
            } else {
                format!("{} (通知)", record.app_identifier)
            };
            tx.send(IncomingMessage {
                kind: super::SourceKind::Notification,
                source,
                text,
                sender: None,
                id: id.clone(),
                received_at: record.delivered_date + CF_EPOCH,
            })
            .await
            .context("通知消息通道已关闭")?;
            state_mutex
                .lock()
                .map_err(|_| anyhow::anyhow!("通知游标锁已损坏"))?
                .processed
                .insert(id, Instant::now());
        }
        {
            let mut state = state_mutex
                .lock()
                .map_err(|_| anyhow::anyhow!("通知游标锁已损坏"))?;
            state.high_water = new_high_water;
            state.last_success = cf_now();
        }
        if healthy {
            reporter.running();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE app(app_id INTEGER PRIMARY KEY, identifier TEXT); CREATE TABLE record(app_id INTEGER, uuid BLOB, data BLOB, delivered_date REAL); INSERT INTO app VALUES(1,'com.synthetic.mail'); INSERT INTO app VALUES(2,'com.synthetic.chat');").unwrap();
        conn
    }

    fn payload(code: &str) -> Vec<u8> {
        let mut req = plist::Dictionary::new();
        req.insert("titl".into(), Value::String("验证码".into()));
        req.insert("body".into(), Value::String(format!("验证码：{code}")));
        let mut root = plist::Dictionary::new();
        root.insert("req".into(), Value::Dictionary(req));
        let mut bytes = Vec::new();
        Value::Dictionary(root)
            .to_writer_binary(&mut bytes)
            .unwrap();
        bytes
    }

    fn insert(conn: &Connection, app: i64, date: f64, uuid: u8) {
        conn.execute(
            "INSERT INTO record VALUES(?1,?2,?3,?4)",
            rusqlite::params![app, vec![uuid], payload("123456"), date],
        )
        .unwrap();
    }

    #[test]
    fn modern_schema_and_binary_plist_are_decoded() {
        let conn = database();
        validate_schema(&conn).unwrap();
        insert(&conn, 1, 101.0, 1);
        let record = query_page(&conn, 0, 0, 100.0).unwrap().remove(0);
        assert_eq!(record.app_identifier, "com.synthetic.mail");
        assert!(notification_text(&record.data).unwrap().contains("123456"));
    }

    #[test]
    fn unsupported_schema_is_explicit_error() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE record(app_id,title,body);")
            .unwrap();
        assert!(validate_schema(&conn)
            .unwrap_err()
            .to_string()
            .contains("schema"));
    }

    #[test]
    fn rowid_catches_late_insert_behind_time_window() {
        let conn = database();
        insert(&conn, 1, 101.0, 1);
        assert_eq!(query_page(&conn, 0, 0, 200.0).unwrap().len(), 1);
    }

    #[test]
    fn pagination_does_not_lose_records_or_mail_displaced_by_other_apps() {
        let conn = database();
        insert(&conn, 1, 101.0, 1);
        for i in 0..455 {
            insert(&conn, 2, 102.0 + i as f64, (i % 255) as u8);
        }
        let mut after = 0;
        let mut count = 0;
        let mut mail = 0;
        loop {
            let rows = query_page(&conn, after, 0, 100.0).unwrap();
            if rows.is_empty() {
                break;
            }
            after = rows.last().unwrap().rowid;
            count += rows.len();
            mail += rows
                .iter()
                .filter(|row| is_mail_app(&row.app_identifier))
                .count();
        }
        assert_eq!(count, 456);
        assert_eq!(mail, 1);
    }

    #[test]
    fn query_failure_does_not_mutate_cursor() {
        let conn = database();
        let state = NotificationState {
            high_water: 100,
            last_success: 100.0,
            ..Default::default()
        };
        conn.execute_batch("DROP TABLE record").unwrap();
        assert!(query_page(&conn, 0, state.high_water, state.last_success).is_err());
        assert_eq!(state.high_water, 100);
        assert_eq!(state.last_success, 100.0);
    }

    #[test]
    fn same_text_different_notification_ids_are_independent() {
        let conn = database();
        insert(&conn, 1, 101.0, 1);
        insert(&conn, 1, 102.0, 2);
        let rows = query_page(&conn, 0, 0, 100.0).unwrap();
        assert_ne!(record_id(&rows[0]), record_id(&rows[1]));
        assert_eq!(
            notification_text(&rows[0].data).unwrap(),
            notification_text(&rows[1].data).unwrap()
        );
    }

    #[test]
    fn title_only_notification_waits_for_body_fill() {
        let mut req = plist::Dictionary::new();
        req.insert("titl".into(), Value::String("Verification code".into()));
        let mut root = plist::Dictionary::new();
        root.insert("req".into(), Value::Dictionary(req));
        let mut bytes = Vec::new();
        Value::Dictionary(root)
            .to_writer_binary(&mut bytes)
            .unwrap();
        assert_eq!(notification_text(&bytes).unwrap(), "");
    }

    #[test]
    fn delivery_sort_leaves_latest_notification_last() {
        let conn = database();
        insert(&conn, 1, 102.0, 2);
        insert(&conn, 1, 101.0, 1);
        let mut rows = query_page(&conn, 0, 0, 100.0).unwrap();
        rows.sort_by(|a, b| {
            a.delivered_date
                .total_cmp(&b.delivered_date)
                .then(a.rowid.cmp(&b.rowid))
        });
        assert_eq!(rows.last().unwrap().delivered_date, 102.0);
    }

    #[test]
    fn database_candidates_cover_new_and_darwin_locations() {
        let paths = database_candidates(
            Path::new("/synthetic/home"),
            Some(Path::new("/synthetic/cache/0")),
        );
        assert!(paths[0].ends_with("group.com.apple.usernoted/db2/db"));
        assert!(paths[1].ends_with("com.apple.notificationcenter/db2/db"));
    }
}
