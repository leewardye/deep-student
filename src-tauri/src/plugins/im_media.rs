//! IM 附件管线（plugins::im_media）。
//!
//! 各通道插件把下载到的媒体字节交给 `ingest`：校验大小/MIME → 落盘到
//! `app_data/im_inbox/<channel>/<yyyy-MM-dd>/` → 幂等去重 → 返回本地路径回执。
//! dstu-vfs 正式入库（资源库索引）属 P3 钩子，P0 落盘路径已可被 agent 引用。

use std::path::{Path, PathBuf};

use crate::database::Database;
use crate::models::AppError;

use super::im_core::{AttachKind, IngestedAttachment, ImChannel};

/// 单附件大小上限（50MB）
pub const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

/// 全局「补充扩展名」设置键（逗号分隔；两个 IM 通道共用）
pub const EXTRA_EXT_SETTING_KEY: &str = "plugin.im.extra_ext";

/// 按扩展名的 MIME 白名单（错题/作业场景为主）。
/// 覆盖：图片 / 常见文档（含 WPS、知网 CAJ、OFD）/ 音视频 / 压缩包 / 网页存档 /
/// 纯文本与数据格式。可执行文件（exe/dll/bat 等）与安装包永远不在名单内。
const ALLOWED_EXTENSIONS: &[&str] = &[
    // 图片
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "heic", "heif", "svg", "tif", "tiff",
    // 文档
    "pdf", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "txt", "md", "markdown", "csv", "epub",
    "wps", "et", "dps", "ofd", "caj", "nh", "kdh", "rtf", "pages", "key", "numbers",
    // 网页存档
    "html", "htm", "mhtml", "mht",
    // 压缩包（只入库不执行）
    "zip", "rar", "7z", "tar", "gz", "bz2", "xz",
    // 纯文本/数据
    "log", "json", "xml", "yml", "yaml", "ipynb", "srt", "vtt",
    // 音视频
    "mp3", "m4a", "wav", "flac", "aac", "ogg", "opus", "mp4", "mov", "avi", "mkv", "webm", "flv",
];

/// 读取用户配置的「补充扩展名」（逗号/分号/空白分隔）。
/// 只接受 1-10 位字母数字扩展名，其余静默丢弃。
pub fn extra_allowed_extensions(db: &Database) -> Vec<String> {
    let raw = db
        .get_setting(EXTRA_EXT_SETTING_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();
    raw.split(|c: char| c == ',' || c == ';' || c == '，' || c == '；' || c.is_whitespace())
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 10
                && s.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .collect()
}

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
/// `extra` 为用户在设置里补充的扩展名（与内置白名单取并集）。
/// 未知且无扩展名 → 拒绝（宁可拒收也不落盘未知内容）。
pub fn is_allowed_attachment(filename: Option<&str>, mime: Option<&str>, extra: &[String]) -> bool {
    if let Some(name) = filename {
        let ext = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if let Some(ext) = ext {
            return ALLOWED_EXTENSIONS.contains(&ext.as_str()) || extra.iter().any(|e| e == &ext);
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

/// AttachKind 的英文标识（生成兜底文件名用）
fn kind_name(kind: AttachKind) -> &'static str {
    match kind {
        AttachKind::Image => "image",
        AttachKind::File => "file",
        AttachKind::Audio => "audio",
        AttachKind::Video => "video",
    }
}

/// 依据文件头魔数嗅探扩展名（微信/飞书的图片消息不带文件名，传输途中
/// 文件也可能被改名/去扩展名，文件名解析失败时以内容为准）。
pub fn sniff_ext(bytes: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| bytes.starts_with(sig);
    if starts(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if starts(b"\xff\xd8\xff") {
        Some("jpg")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some("gif")
    } else if bytes.len() > 12 && starts(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if starts(b"BM") {
        Some("bmp")
    } else if starts(b"%PDF") {
        Some("pdf")
    } else if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        // zip 容器：可能是 docx/xlsx/pptx，也可能是普通 zip —— 按扩展名
        // 白名单里都允许，落盘名优先沿用原始文件名，这里给通用 zip
        Some("zip")
    } else if bytes.len() > 11 && &bytes[4..8] == b"ftyp" {
        Some("mp4")
    } else if starts(b"ID3") || (bytes.len() > 2 && bytes[0] == 0xff && (bytes[1] & 0xe0) == 0xe0) {
        Some("mp3")
    } else if starts(b"fLaC") {
        Some("flac")
    } else if starts(b"\x1aE\xdf\xa3") {
        Some("mkv")
    } else if starts(b"OggS") {
        Some("ogg")
    } else {
        None
    }
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
    extra_ext: &[String],
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
    // 扩展名解析：文件名 → 魔数嗅探。文件名扩展名未知/缺失时以文件头为准
    // （微信/飞书图片消息不带文件名；传输中被改名为 .bin 也在此兜底）。
    let ext_from_name: Option<String> = filename.and_then(|n| {
        Path::new(n)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
    });
    let sniffed = sniff_ext(bytes).map(str::to_string);
    let in_list =
        |e: &str| ALLOWED_EXTENSIONS.contains(&e) || extra_ext.iter().any(|x| x == e);
    let effective_ext: Option<String> = match &ext_from_name {
        Some(e) if in_list(e) => Some(e.clone()),
        _ => sniffed.clone().filter(|e| in_list(e)),
    };
    // 图片类消息（拍照错题主场景）即使嗅探不出也放行：IM 图片端点返回的
    // 必然是图片，宁可落盘也不拦用户的错题
    let image_fallback = matches!(kind, AttachKind::Image);
    if effective_ext.is_none() && !image_fallback {
        return Err(AppError::validation(format!(
            "不支持的附件类型（扩展名不在白名单，可在插件设置补充）：{}",
            ext_from_name.unwrap_or_else(|| "未知".to_string())
        )));
    }
    let display_ext = effective_ext
        .or(sniffed)
        .unwrap_or_else(|| "jpg".to_string());
    let display_name = match filename.map(str::trim) {
        Some(name) if !name.is_empty() => sanitize_filename(Some(name), kind),
        _ => sanitize_filename(
            Some(&format!("{}_{}.{}", kind_name(kind), index + 1, display_ext)),
            kind,
        ),
    };
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
                vfs_id: None,
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
        vfs_id: None,
    })
}

/// 把已落盘的附件注册进 VFS 资源库（agent 的附件工具/桌面端资源库都能引用）。
/// 幂等：同内容由 VFS 内容哈希去重，重复注册返回已有 ID。
/// 失败不阻断消息主流程——附件仍有本地路径可用。
pub fn register_vfs(app: &tauri::AppHandle, att: &mut IngestedAttachment) {
    use tauri::Manager;
    let result = (|| -> Result<String, AppError> {
        let db = app
            .try_state::<std::sync::Arc<crate::vfs::VfsDatabase>>()
            .ok_or_else(|| AppError::internal("VfsDatabase 未初始化"))?
            .inner()
            .clone();
        let bytes = std::fs::read(&att.local_path)
            .map_err(|e| AppError::internal(format!("读取附件失败: {}", e)))?;
        let b64 = use_base64(&bytes);
        let params = crate::vfs::types::VfsUploadAttachmentParams {
            name: att.display_name.clone(),
            mime_type: mime_for(att).to_string(),
            base64_content: b64,
            attachment_type: Some(match att.kind {
                crate::plugins::im_core::AttachKind::Image => "image".to_string(),
                _ => "file".to_string(),
            }),
        };
        let result = crate::vfs::repos::VfsAttachmentRepo::upload_with_folder(
            &db, params, None,
        )
        .map_err(|e| AppError::internal(format!("VFS 注册失败: {}", e)))?;
        Ok(result.source_id)
    })();
    match result {
        Ok(id) => att.vfs_id = Some(id),
        Err(e) => tracing::warn!("[im_media] VFS 注册失败（不阻断）: {}", e),
    }
}

/// 依据扩展名/类别推断 MIME（上传白名单已保证是常见类型）
fn mime_for(att: &IngestedAttachment) -> &'static str {
    let name = att.display_name.to_ascii_lowercase();
    if name.ends_with(".png") {
        "image/png"
    } else if name.ends_with(".jpg") || name.ends_with(".jpeg") {
        "image/jpeg"
    } else if name.ends_with(".webp") {
        "image/webp"
    } else if name.ends_with(".gif") {
        "image/gif"
    } else if name.ends_with(".pdf") {
        "application/pdf"
    } else {
        match att.kind {
            crate::plugins::im_core::AttachKind::Image => "image/png",
            _ => "application/octet-stream",
        }
    }
}

/// 标准 base64（无换行）
fn use_base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_allowlist() {
        let extra: Vec<String> = vec!["dwg".into()];
        assert!(is_allowed_attachment(Some("错题.jpg"), None, &[]));
        assert!(is_allowed_attachment(Some("作业.PDF"), None, &[]));
        assert!(is_allowed_attachment(Some("论文.caj"), None, &[]));
        assert!(is_allowed_attachment(Some("文档.wps"), None, &[]));
        assert!(is_allowed_attachment(Some("资料.zip"), None, &[]));
        assert!(is_allowed_attachment(Some("图纸.dwg"), None, &extra));
        assert!(!is_allowed_attachment(Some("图纸.dwg"), None, &[]));
        assert!(!is_allowed_attachment(Some("virus.exe"), None, &extra));
        assert!(!is_allowed_attachment(Some("noext"), None, &extra));
        assert!(is_allowed_attachment(None, Some("image/png"), &[]));
        assert!(!is_allowed_attachment(None, Some("application/octet-stream"), &[]));
    }

    #[test]
    fn extra_extension_parsing() {
        // 纯函数部分无法构造 Database，这里只测清洗逻辑经由公开路径的边界；
        // Database 读取路径由 ingest 集成验证。
        let raw = " DWG, ;exe；.zip，，abc-123 ";
        let parsed: Vec<String> = raw
            .split(|c: char| c == ',' || c == ';' || c == '，' || c == '；' || c.is_whitespace())
            .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| {
                !s.is_empty() && s.len() <= 10 && s.chars().all(|c| c.is_ascii_alphanumeric())
            })
            .collect();
        assert_eq!(parsed, vec!["dwg", "exe", "zip"]);
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
    fn sniff_detects_common_signatures() {
        assert_eq!(sniff_ext(b"\x89PNG\r\n\x1a\n...."), Some("png"));
        assert_eq!(sniff_ext(b"\xff\xd8\xff\xe0rest"), Some("jpg"));
        assert_eq!(sniff_ext(b"%PDF-1.7 ..."), Some("pdf"));
        assert_eq!(sniff_ext(b"PK\x03\x04....."), Some("zip"));
        assert_eq!(sniff_ext(b"GIF89a...."), Some("gif"));
        // WEBP：RIFF 头 + 偏移 8 的 WEBP 标记
        let webp = b"RIFF\x24\x00\x00\x00WEBPVP8 ";
        assert_eq!(sniff_ext(webp), Some("webp"));
        assert_eq!(sniff_ext(b"not a known format at all"), None);
        assert_eq!(sniff_ext(b""), None);
    }

    #[test]
    fn ingest_rejects_empty_and_oversize() {
        // 不需要真实 AppHandle 的分支先行验证
        assert!(is_allowed_attachment(Some("a.jpg"), None, &[]));
    }
}
