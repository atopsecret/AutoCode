//! 小型 RFC 822/MIME 正文适配器。只处理文本部分；附件不参与验证码提取。
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::collections::HashMap;

const MAX_MAIL_BYTES: usize = 10 * 1024 * 1024;
const MAX_MIME_DEPTH: usize = 16;

#[derive(Debug)]
pub(crate) struct MailContent {
    pub text: String,
    pub sender: Option<String>,
    pub message_id: Option<String>,
    pub received_at: Option<f64>,
}

fn header_body(raw: &[u8]) -> Result<(HashMap<String, String>, &[u8])> {
    let (end, separator) = raw
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map(|pos| (pos, 4))
        .or_else(|| {
            raw.windows(2)
                .position(|part| part == b"\n\n")
                .map(|pos| (pos, 2))
        })
        .context("邮件头尚未完整写入或缺少头部/正文分隔")?;
    let text = std::str::from_utf8(&raw[..end]).context("邮件头不是有效 UTF-8/ASCII")?;
    let mut unfolded = Vec::<String>::new();
    for line in text.lines() {
        if line.starts_with([' ', '\t']) {
            unfolded
                .last_mut()
                .context("邮件折叠头缺少前置字段")?
                .push_str(&format!(" {}", line.trim()));
        } else {
            unfolded.push(line.trim_end_matches('\r').to_owned());
        }
    }
    let mut headers = HashMap::new();
    for line in unfolded {
        let (name, value) = line.split_once(':').context("邮件头字段缺少冒号")?;
        headers
            .entry(name.trim().to_ascii_lowercase())
            .or_insert_with(|| value.trim().to_owned());
    }
    Ok((headers, &raw[end + separator..]))
}

fn parameter(value: &str, key: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|part| {
        let (name, content) = part.trim().split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case(key)
            .then(|| content.trim().trim_matches('"').to_owned())
    })
}

fn quoted_printable(raw: &[u8]) -> Result<Vec<u8>> {
    let mut result = Vec::with_capacity(raw.len());
    let mut pos = 0;
    while pos < raw.len() {
        if raw[pos] != b'=' {
            result.push(raw[pos]);
            pos += 1;
            continue;
        }
        if raw.get(pos + 1) == Some(&b'\n') {
            pos += 2;
            continue;
        }
        if raw.get(pos + 1..pos + 3) == Some(b"\r\n") {
            pos += 3;
            continue;
        }
        let digits = raw
            .get(pos + 1..pos + 3)
            .context("quoted-printable 尾部尚未写完")?;
        let hex = std::str::from_utf8(digits).context("quoted-printable 包含非ASCII转义")?;
        result.push(u8::from_str_radix(hex, 16).context("quoted-printable 转义无效")?);
        pos += 3;
    }
    Ok(result)
}

fn decode_charset(raw: Vec<u8>, charset: &str) -> Result<String> {
    match charset.to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" | "us-ascii" | "ascii" => {
            String::from_utf8(raw).context("文本部分不是声明的 UTF-8/ASCII")
        }
        "iso-8859-1" | "latin1" | "latin-1" => Ok(raw.into_iter().map(char::from).collect()),
        other => anyhow::bail!("邮件字符集 {other} 尚不受支持；未将乱码当验证码"),
    }
}

fn decode_header(value: &str) -> Result<String> {
    let mut result = String::new();
    let mut remaining = value;
    while let Some(start) = remaining.find("=?") {
        result.push_str(&remaining[..start]);
        let encoded = &remaining[start + 2..];
        let (charset, encoded) = encoded
            .split_once('?')
            .context("RFC2047 字符集字段不完整")?;
        let (encoding, encoded) = encoded.split_once('?').context("RFC2047 编码字段不完整")?;
        let (payload, tail) = encoded.split_once("?=").context("RFC2047 编码头不完整")?;
        let bytes = match encoding.to_ascii_lowercase().as_str() {
            "b" => STANDARD.decode(payload).context("RFC2047 base64 无效")?,
            "q" => quoted_printable(payload.replace('_', " ").as_bytes())?,
            _ => anyhow::bail!("RFC2047 编码方式不受支持"),
        };
        result.push_str(&decode_charset(bytes, charset)?);
        remaining = if tail.trim_start().starts_with("=?") {
            tail.trim_start()
        } else {
            tail
        };
    }
    result.push_str(remaining);
    Ok(result)
}

fn mime_text(raw: &[u8], depth: usize) -> Result<Vec<(String, String)>> {
    anyhow::ensure!(depth <= MAX_MIME_DEPTH, "MIME 嵌套过深");
    let (headers, body) = header_body(raw)?;
    let content_type = headers
        .get("content-type")
        .map(String::as_str)
        .unwrap_or("text/plain; charset=utf-8");
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or("text/plain")
        .trim()
        .to_ascii_lowercase();
    let disposition = headers
        .get("content-disposition")
        .map(String::as_str)
        .unwrap_or("");
    if disposition.to_ascii_lowercase().starts_with("attachment")
        || parameter(disposition, "filename").is_some()
    {
        return Ok(Vec::new());
    }
    if media_type.starts_with("multipart/") {
        let boundary = parameter(content_type, "boundary").context("multipart 缺少 boundary")?;
        anyhow::ensure!(
            !boundary.is_empty() && boundary.len() <= 200,
            "MIME boundary 无效"
        );
        let marker = format!("--{boundary}");
        let ending = format!("--{boundary}--");
        let mut parts = Vec::new();
        let mut current = Vec::new();
        let mut inside = false;
        let mut closed = false;
        for line in body.split_inclusive(|byte| *byte == b'\n') {
            let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
            let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
            if trimmed == marker.as_bytes() || trimmed == ending.as_bytes() {
                if inside && !current.is_empty() {
                    parts.extend(mime_text(&current, depth + 1)?);
                    current.clear();
                }
                inside = true;
                if trimmed == ending.as_bytes() {
                    closed = true;
                    break;
                }
            } else if inside {
                current.extend_from_slice(line);
            }
        }
        anyhow::ensure!(closed, "multipart 尾部 boundary 尚未写完");
        if media_type == "multipart/alternative" {
            if let Some(index) = parts.iter().position(|(kind, _)| kind == "text/plain") {
                return Ok(vec![parts.remove(index)]);
            }
            if let Some(index) = parts.iter().position(|(kind, _)| kind == "text/html") {
                return Ok(vec![parts.remove(index)]);
            }
        }
        return Ok(parts);
    }
    if media_type != "text/plain" && media_type != "text/html" {
        return Ok(Vec::new());
    }
    let encoding = headers
        .get("content-transfer-encoding")
        .map(String::as_str)
        .unwrap_or("8bit");
    let decoded = match encoding.to_ascii_lowercase().as_str() {
        "base64" => STANDARD
            .decode(
                body.iter()
                    .copied()
                    .filter(|byte| !byte.is_ascii_whitespace())
                    .collect::<Vec<_>>(),
            )
            .context("MIME base64 尚未完整写入或无效")?,
        "quoted-printable" => quoted_printable(body)?,
        "7bit" | "8bit" | "binary" => body.to_vec(),
        _ => anyhow::bail!("MIME transfer encoding 不受支持"),
    };
    let charset = parameter(content_type, "charset").unwrap_or_else(|| "utf-8".into());
    Ok(vec![(
        media_type,
        decode_charset(decoded, &charset)?.trim().to_owned(),
    )])
}

pub(crate) fn parse_emlx(raw: &[u8]) -> Result<MailContent> {
    let newline = raw
        .iter()
        .position(|byte| *byte == b'\n')
        .context("emlx 字节计数尚未写完")?;
    let count: usize = std::str::from_utf8(&raw[..newline])?
        .trim()
        .parse()
        .context("emlx 首行不是字节计数")?;
    anyhow::ensure!(count <= MAX_MAIL_BYTES, "emlx 邮件超过10MiB文本解析上限");
    let end = (newline + 1).checked_add(count).context("emlx 长度溢出")?;
    let message = raw
        .get(newline + 1..end)
        .context("emlx 邮件字节尚未完整写入")?;
    parse_rfc822(message)
}

pub(crate) fn parse_rfc822(raw: &[u8]) -> Result<MailContent> {
    anyhow::ensure!(raw.len() <= MAX_MAIL_BYTES, "邮件超过10MiB文本解析上限");
    let (headers, _) = header_body(raw)?;
    let subject = headers
        .get("subject")
        .map(|value| decode_header(value))
        .transpose()?
        .unwrap_or_default();
    let body = mime_text(raw, 0)?
        .into_iter()
        .map(|(_, text)| text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let sender = headers.get("from").map(|value| {
        let value = match (value.find('<'), value.rfind('>')) {
            (Some(start), Some(end)) if start < end => &value[start + 1..end],
            _ => value.as_str(),
        };
        value.trim().to_ascii_lowercase()
    });
    let message_id = headers
        .get("message-id")
        .map(|value| value.trim().trim_matches(['<', '>']).to_owned())
        .filter(|value| !value.is_empty());
    let received_at = headers
        .get("date")
        .and_then(|value| chrono::DateTime::parse_from_rfc2822(value).ok())
        .map(|date| date.timestamp_millis() as f64 / 1000.0);
    Ok(MailContent {
        text: [subject, body]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        sender,
        message_id,
        received_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emlx_byte_count_excludes_metadata_and_extracts_sender() {
        let message =
            "From: Synthetic <no-reply@example.invalid>\r\nSubject: 验证\r\n\r\n验证码123456";
        let raw = format!("{}\n{message}<plist>订单999999</plist>", message.len());
        let parsed = parse_emlx(raw.as_bytes()).unwrap();
        assert_eq!(parsed.sender.as_deref(), Some("no-reply@example.invalid"));
        assert!(parsed.text.contains("123456"));
        assert!(!parsed.text.contains("999999"));
    }

    #[test]
    fn incomplete_emlx_is_retryable_error() {
        assert!(parse_emlx(b"100\nFrom: sample\n\nshort").is_err());
    }

    #[test]
    fn base64_utf8_body_is_decoded() {
        let encoded = STANDARD.encode("验证码123456");
        let raw = format!("From: sample@example.invalid\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{encoded}");
        assert_eq!(parse_rfc822(raw.as_bytes()).unwrap().text, "验证码123456");
    }

    #[test]
    fn quoted_printable_and_soft_line_break_are_decoded() {
        let raw = b"From: sample@example.invalid\nContent-Type: text/plain\nContent-Transfer-Encoding: quoted-printable\n\nVerification=20code:=20123=\n456";
        assert_eq!(parse_rfc822(raw).unwrap().text, "Verification code: 123456");
    }

    #[test]
    fn multipart_prefers_plain_and_ignores_attachments() {
        let raw = b"From: sample@example.invalid\nContent-Type: multipart/mixed; boundary=outer\n\n--outer\nContent-Type: multipart/alternative; boundary=inner\n\n--inner\nContent-Type: text/plain\n\nVerification code: 123456\n--inner\nContent-Type: text/html\n\n<b>654321</b>\n--inner--\n--outer\nContent-Type: text/plain\nContent-Disposition: attachment; filename=receipt.txt\n\nOrder 999999\n--outer--\n";
        let text = parse_rfc822(raw).unwrap().text;
        assert_eq!(text, "Verification code: 123456");
    }

    #[test]
    fn encoded_subject_and_folded_headers_are_supported() {
        let raw = b"From: Synthetic\n <sample@example.invalid>\nSubject: =?UTF-8?Q?Verification_code?=\n\n123456";
        let parsed = parse_rfc822(raw).unwrap();
        assert_eq!(parsed.sender.as_deref(), Some("sample@example.invalid"));
        assert_eq!(parsed.text, "Verification code\n123456");
    }
}
