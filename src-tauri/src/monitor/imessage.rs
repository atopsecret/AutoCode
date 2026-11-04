use anyhow::{Context, Result};
use rusqlite::Connection;
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::time::{self, Duration};

use super::{IncomingMessage, MessageSender, SourceReporter};

// -1 区分首次启动与合法空数据库；监督者重启时不重新跳到 MAX(ROWID)。
static LAST_ROWID: AtomicI64 = AtomicI64::new(-1);

#[derive(Debug)]
struct MessageRow {
    rowid: i64,
    text: Option<String>,
    attributed_body: Option<Vec<u8>>,
    sender: Option<String>,
    date: f64,
}

fn db_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("无法获取 HOME 环境变量")?;
    Ok(PathBuf::from(home).join("Library/Messages/chat.db"))
}

fn query_new_messages(conn: &Connection, since_rowid: i64) -> Result<Vec<MessageRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT m.ROWID, m.text, m.attributedBody, h.id, m.date
         FROM message m LEFT JOIN handle h ON m.handle_id = h.ROWID
         WHERE m.ROWID > ?1 AND COALESCE(m.is_from_me, 0) = 0
         ORDER BY m.ROWID ASC LIMIT 200",
        )
        .context("Messages schema 不受支持；需要 text/attributedBody/date/is_from_me 字段")?;
    let rows = stmt.query_map([since_rowid], |row| {
        Ok(MessageRow {
            rowid: row.get(0)?,
            text: row.get(1)?,
            attributed_body: row.get(2)?,
            sender: row.get(3)?,
            date: row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("读取 Messages 记录失败")
}

fn message_text(row: &MessageRow) -> Result<Option<String>> {
    if let Some(text) = &row.text {
        if !text.trim().is_empty() {
            return Ok(Some(text.clone()));
        }
    }
    if row
        .attributed_body
        .as_ref()
        .is_some_and(|data| !data.is_empty())
    {
        // macOS 的 NSAttributedString typedstream 不是普通 UTF-8 或 plist。
        // 未引入经过验证的 typedstream 解码器前，不从二进制中搜索数字猜验证码。
        anyhow::bail!("此 Messages 记录只有 attributedBody，当前归档编码不受支持；未猜测其内容");
    }
    Ok(None)
}

fn delivery_time(date: f64) -> f64 {
    let seconds = if date.abs() > 1_000_000_000_000.0 {
        date / 1_000_000_000.0
    } else {
        date
    };
    seconds + 978_307_200.0
}

pub async fn monitor(tx: MessageSender, reporter: SourceReporter) -> Result<()> {
    let mut conn = tokio::task::spawn_blocking(|| -> Result<Connection> {
        let path = db_path()?;
        let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("无法只读打开 Messages 数据库；目录不存在和访问被拒绝请分别检查")?;
        conn.busy_timeout(Duration::from_secs(1))?;
        let max: i64 = conn.query_row("SELECT COALESCE(MAX(ROWID),0) FROM message", [], |row| {
            row.get(0)
        })?;
        let old = LAST_ROWID.load(Ordering::SeqCst);
        if old < 0 || max < old {
            LAST_ROWID.store(max, Ordering::SeqCst);
        }
        // 初始化时也验证实际使用的 schema，而不是仅把“文件可打开”当运行成功。
        query_new_messages(&conn, max)?;
        Ok(conn)
    })
    .await
    .context("Messages 初始化任务失败")??;
    reporter.running();
    let mut interval = time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let last = LAST_ROWID.load(Ordering::SeqCst);
        let (returned_conn, rows) = tokio::task::spawn_blocking(move || {
            let rows = query_new_messages(&conn, last);
            (conn, rows)
        })
        .await
        .context("Messages 查询任务失败")?;
        conn = returned_conn;
        let mut healthy = true;
        for row in rows? {
            let text = match message_text(&row) {
                Ok(Some(text)) => text,
                Ok(None) => {
                    LAST_ROWID.fetch_max(row.rowid, Ordering::SeqCst);
                    continue;
                }
                Err(error) => {
                    reporter.error(&format!("Messages 记录 {}：{error}", row.rowid));
                    healthy = false;
                    LAST_ROWID.fetch_max(row.rowid, Ordering::SeqCst);
                    continue;
                }
            };
            tx.send(IncomingMessage {
                kind: super::SourceKind::Imessage,
                source: "iMessage".into(),
                text,
                sender: row.sender,
                id: format!("imessage:{}", row.rowid),
                received_at: delivery_time(row.date),
            })
            .await
            .context("Messages 消息通道已关闭")?;
            LAST_ROWID.fetch_max(row.rowid, Ordering::SeqCst);
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
    fn outgoing_messages_are_not_ingested() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE handle(id TEXT); CREATE TABLE message(text TEXT, attributedBody BLOB, handle_id INTEGER, date REAL, is_from_me INTEGER); INSERT INTO handle VALUES('synthetic'); INSERT INTO message VALUES('验证码123456',NULL,1,10,1); INSERT INTO message VALUES('验证码654321',NULL,1,11,0);").unwrap();
        let rows = query_new_messages(&conn, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text.as_deref(), Some("验证码654321"));
    }

    #[test]
    fn attributed_archive_is_reported_not_guessed() {
        let row = MessageRow {
            rowid: 1,
            text: None,
            attributed_body: Some(b"arbitrary 123456 binary".to_vec()),
            sender: None,
            date: 0.0,
        };
        assert!(message_text(&row)
            .unwrap_err()
            .to_string()
            .contains("attributedBody"));
    }

    #[test]
    fn cf_seconds_and_nanoseconds_have_same_delivery_time() {
        assert_eq!(
            delivery_time(750_000_000.0),
            delivery_time(750_000_000_000_000_000.0)
        );
    }
}
