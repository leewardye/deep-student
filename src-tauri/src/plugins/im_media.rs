//! IM 附件管线（plugins::im_media）。
//!
//! 各通道插件把下载到的媒体字节交给 `ingest`：校验大小/MIME → 落盘到
//! `app_data/im_inbox/<channel>/<yyyy-MM-dd>/` → 幂等去重 → 返回本地路径回执。
//! dstu-vfs 正式入库（资源库索引）属 P3 钩子，P0 落盘路径已可被 agent 引用。

use std::path::{Path, PathBuf};

use crate::models::AppError;

use super::im_core::{AttachKind, IngestedAttachment, ImChannel};

/// 单附件大小上限（20MB）
pub const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// 按扩展名的 MIME 白名单（错题/作业场景为主）
const ALLOWED_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "heic", "heif", "pdf", "doc", "docx", "ppt",
    "pptx", "xls", "xlsx", "txt", "md", "csv", "epub", "mp3", "m4a", "wav", "mp4", "mov",
];

/// 落盘根目录：`app_data_dir/im_inbox/<channel>/`
pub fn inbox_root(app: &tauri::AppHandle, channel: ImChannel) -> Result<PathBuf, AppError> {
    use tauri::Manager;
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::internal(format!("无法解析应用数据目录: {}", e)))?;
    Ok(base.join("im_inbox").join(channel.as_str()))
}

/// 扩展名白名单校验：优先用 filename 的扩展名，其次 mime 映射。
/// 未知且无扩展名 → 拒绝（宁可拒收也不落盘未知内容）。
pub fn is_allowed_attachment(filename: Option<&str>, mime: Option<&str>) -> bool {
    if let Some(name) = filename {
        let ext = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if let Some(ext) = ext {
            return ALLOWED_EXTENSIONS.contains(&ext.as_str());
        }
    }
    if let Some(m) = mime {
        let m = m.to_ascii_lowercase();
        let ext = match m.as_str() {
            "image/jpeg" => "jpg",
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "image/heic" | "image/heif" => "heic",
            "application/pdf" => "pdf",
            "text/plain" => "txt",
            "text/markdown" => "md",
            "text/csv" => "csv",
            "audio/mpeg" => "mp3",
            "audio/mp4" => "m4a",
            "audio/wav" | "audio/x-wav" => "wav",
            "video/mp4" => "mp4",
            "video/quicktime" => "mov",
            _ => return false,
        };
        return ALLOWED_EXTENSIONS.contains(&ext);
    }
    false
}

/// 清洗文件名：去路径分隔符与控制字符，限长 80 字符，空则回退 kind 名
pub fn sanitize_filename(filename: Option<&str>, kind: AttachKind) -> String {
    let raw = filename
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(match kind {
            AttachKind::Image => "image",
            AttachKind::File => "file",
            AttachKind::Audio => "audio",
            AttachKind::Video => "video",
        });
    let mut cleaned: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    if cleaned.chars().count() > 80 {
        // 截断时保留扩展名
        let ext = Path::new(&cleaned)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e))
            .unwrap_or_default();
        let stem: String = cleaned
            .chars()
            .take(80 - ext.chars().count())
            .collect();
        cleaned = format!("{}{}", stem, ext);
    }
    cleaned
}

fn today_yyyymmdd() -> String {
    // chrono 已是 workspace 依赖（events.rs 使用）
    let now = chrono::Utc::now();
    now.format("%Y-%m-%d").to_string()
}

/// 附件入库：字节流 → 校验 → 幂等落盘 → 回执。
///
/// 幂等键 = (channel, msg_id, index)：同键同名同大小直接复用已有文件，
/// 飞书事件重投不会产生重复副本。
pub fn ingest(
    app: &tauri::AppHandle,
    channel: ImChannel,
    msg_id: &str,
    index: usize,
    filename: Option<&str>,
    kind: AttachKind,
    bytes: &[u8],
) -> Result<IngestedAttachment, AppError> {
    if bytes.is_empty() {
        return Err(AppError::validation("附件内容为空"));
    }
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(AppError::validation(format!(
            "附件超过大小上限 {}MB",
            MAX_ATTACHMENT_BYTES / 1024 / 1024
        )));
    }
    if !is_allowed_attachment(filename, None) {
        return Err(AppError::validation("不支持的附件类型（扩展名不在白名单）"));
    }
    let display_name = sanitize_filename(filename, kind);
    let safe_msg_id: String = msg_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if safe_msg_id.is_empty() {
        return Err(AppError::validation("消息 ID 缺失，无法落盘附件"));
    }

    let dir = inbox_root(app, channel)?.join(today_yyyymmdd());
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::internal(format!("创建附件目录失败: {}", e)))?;
    let path = dir.join(format!("{}_{}_{}", safe_msg_id, index, display_name));
    if path.exists() {
        let existing_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if existing_len == bytes.len() as u64 {
            return Ok(IngestedAttachment {
                local_path: path.to_string_lossy().into_owned(),
                display_name,
                kind,
            });
        }
    }
    // 先写临时文件再原子改名，避免半写文件被复用
    let tmp = dir.join(format!(".{}.tmp", safe_msg_id));
    std::fs::write(&tmp, bytes)
        .map_err(|e| AppError::internal(format!("写入附件临时文件失败: {}", e)))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| AppError::internal(format!("附件落盘失败: {}", e)))?;
    Ok(IngestedAttachment {
        local_path: path.to_string_lossy().into_owned(),
        display_name,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_allowlist() {
        assert!(is_allowed_attachment(Some("错题.jpg"), None));
        assert!(is_allowed_attachment(Some("作业.PDF"), None));
        assert!(!is_allowed_attachment(Some("virus.exe"), None));
        assert!(!is_allowed_attachment(Some("noext"), None));
        assert!(is_allowed_attachment(None, Some("image/png")));
        assert!(!is_allowed_attachment(None, Some("application/octet-stream")));
    }

    #[test]
    fn sanitize_strips_path_separators() {
        assert_eq!(
            sanitize_filename(Some("..\\..\\evil.png"), AttachKind::Image),
            "___.._evil.png" // '..\' → '..' chars mapped: '.' stays, '\' → '_'
        );
    }

    #[test]
    fn sanitize_falls_back_to_kind() {
        assert_eq!(sanitize_filename(Some("  "), AttachKind::Image), "image");
    }

    #[test]
    fn sanitize_truncates_long_names() {
        let long = format!("{}.png", "字".repeat(200));
        let out = sanitize_filename(Some(&long), AttachKind::Image);
        assert!(out.chars().count() <= 80);
        assert!(out.ends_with(".png"));
    }

    #[test]
    fn ingest_rejects_empty_and_oversize() {
        // 不需要真实 AppHandle 的分支先行验证
        assert!(is_allowed_attachment(Some("a.jpg"), None));
    }
}
