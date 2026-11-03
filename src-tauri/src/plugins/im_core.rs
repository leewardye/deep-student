//! IM 通道统一消息模型（plugins::im_core）。
//!
//! 目标：飞书 / 微信 iLink / 未来企微等通道插件共用一套「归一化消息」与
//! 「统一出站」结构；协议差异（字段名、媒体引用方式、分片上限）全部留在
//! 各插件内部，业务语义（绑定校验、限流、附件入库、任务路由）只依赖本模块。

use serde::{Deserialize, Serialize};

/// 支持的 IM 通道标识（同时用于日志、来源标签与 RemoteBinding.channel）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImChannel {
    Feishu,
    WechatIlink,
}

impl ImChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            ImChannel::Feishu => "feishu",
            ImChannel::WechatIlink => "ilink-wechat",
        }
    }
}

/// 附件类别（决定 im_media 的落盘扩展名与 MIME 推断）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachKind {
    Image,
    File,
    Audio,
    Video,
}

impl AttachKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AttachKind::Image => "image",
            AttachKind::File => "file",
            AttachKind::Audio => "audio",
            AttachKind::Video => "video",
        }
    }
}

/// 通道侧的附件引用（仅记录引用，不携带字节流）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundAttachment {
    pub kind: AttachKind,
    /// 通道内媒体引用：飞书 = image_key / file_key + message_id 下载；
    /// 微信 iLink = CDN 参数（spike 后定）
    pub media_ref: String,
    pub filename: Option<String>,
    pub mime: Option<String>,
    pub size: Option<u64>,
}

/// 归一化入站消息：所有通道插件把原生事件转成此结构后再进入公共链路
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedInbound {
    pub channel: ImChannel,
    /// 通道内消息唯一 ID（幂等键，空 = 不去重）
    pub msg_id: String,
    /// 发送者 ID（绑定校验用）
    pub peer_id: String,
    /// 会话 ID（出站寻址用）
    pub chat_id: String,
    /// 纯文本正文（图片/文件的 caption 也并入此处）
    pub text: Option<String>,
    pub attachments: Vec<InboundAttachment>,
    /// 消息创建时间（毫秒时间戳，缺省 0）
    pub sent_at_ms: u64,
}

impl UnifiedInbound {
    pub fn is_empty_text(&self) -> bool {
        self.text.as_deref().map(str::trim).unwrap_or("").is_empty()
    }
}

/// 统一出站消息：文本由各插件按通道上限分片；图片为本地路径，
/// 插件负责上传换取通道引用（飞书 image_key / 微信 CDN）
#[derive(Debug, Clone, Default)]
pub struct UnifiedOutbound {
    pub chat_id: String,
    pub text: Option<String>,
    pub images: Vec<std::path::PathBuf>,
    pub reply_to: Option<String>,
}

/// Unicode 安全的按字符数分片（与 ilink client::chunk_text 语义一致，供所有通道复用）
pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for part in text.split_inclusive('\n') {
        for ch in part.chars() {
            if current_len >= limit {
                chunks.push(std::mem::take(&mut current));
                current_len = 0;
            }
            current.push(ch);
            current_len += 1;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// 已入库附件的回执（im_media::ingest 的产物）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestedAttachment {
    pub local_path: String,
    pub display_name: String,
    pub kind: AttachKind,
}

/// 附件清单文本：附加到用户消息后，让 headless agent 知道有哪些落盘文件。
/// P0 约定：headless 工具面为只读白名单，agent 至少能把路径报告给用户/引用到会话。
pub fn attachment_manifest(items: &[IngestedAttachment]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n[附件已入库，共 ");
    out.push_str(&items.len().to_string());
    out.push_str(" 个]");
    for (i, it) in items.iter().enumerate() {
        out.push_str(&format!(
            "\n{}. [{}] {} → {}",
            i + 1,
            it.kind.as_str(),
            it.display_name,
            it.local_path
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_text_preserves_unicode_and_order() {
        let text = "a你\n好\nworld世界";
        let chunks = chunk_text(text, 3);
        let joined: String = chunks.concat();
        assert_eq!(joined, text);
        for c in &chunks {
            assert!(c.chars().count() <= 3);
        }
    }

    #[test]
    fn chunk_text_empty_yields_single_empty() {
        assert_eq!(chunk_text("", 10), vec![String::new()]);
    }

    #[test]
    fn manifest_empty_when_no_items() {
        assert_eq!(attachment_manifest(&[]), "");
    }

    #[test]
    fn manifest_lists_paths() {
        let items = vec![IngestedAttachment {
            local_path: "/tmp/a.jpg".into(),
            display_name: "错题.jpg".into(),
            kind: AttachKind::Image,
        }];
        let m = attachment_manifest(&items);
        assert!(m.contains("[附件已入库，共 1 个]"));
        assert!(m.contains("/tmp/a.jpg"));
        assert!(m.contains("image"));
    }

    #[test]
    fn channel_strings_are_stable() {
        // 序列化字段名是跨端契约，改动前必须同步前端/文档
        assert_eq!(ImChannel::Feishu.as_str(), "feishu");
        assert_eq!(ImChannel::WechatIlink.as_str(), "ilink-wechat");
    }
}
