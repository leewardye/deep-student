//! iLink Bot HTTP client (WeChat ClawBot protocol).

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::Rng;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

use crate::models::AppError;

pub const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
pub const CHANNEL_VERSION: &str = "1.0.0";
pub const DEFAULT_LONG_POLL_TIMEOUT_MS: u64 = 35_000;
pub const SESSION_EXPIRED: i64 = -14;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IlinkCredentials {
    pub token: String,
    pub base_url: String,
    pub account_id: String,
    pub user_id: String,
    #[serde(default)]
    pub get_updates_buf: String,
    /// peer_id -> context_token
    #[serde(default)]
    pub context_tokens: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QrcodeResponse {
    pub qrcode: String,
    pub qrcode_img_content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QrcodeStatusResponse {
    pub status: String,
    pub bot_token: Option<String>,
    pub ilink_bot_id: Option<String>,
    pub ilink_user_id: Option<String>,
    pub baseurl: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetUpdatesResponse {
    pub ret: Option<i64>,
    pub errcode: Option<i64>,
    pub errmsg: Option<String>,
    #[serde(default)]
    pub msgs: Vec<WeixinMessage>,
    pub get_updates_buf: Option<String>,
    pub longpolling_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WeixinMessage {
    pub seq: Option<u64>,
    pub message_id: Option<u64>,
    pub from_user_id: Option<String>,
    pub to_user_id: Option<String>,
    pub client_id: Option<String>,
    pub create_time_ms: Option<u64>,
    pub session_id: Option<String>,
    pub message_type: Option<i32>,
    pub message_state: Option<i32>,
    pub context_token: Option<String>,
    #[serde(default)]
    pub item_list: Vec<MessageItem>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageItem {
    #[serde(rename = "type")]
    pub item_type: Option<i32>,
    pub text_item: Option<TextItem>,
    #[serde(default)]
    pub image_item: Option<ImageItem>,
    #[serde(default)]
    pub file_item: Option<FileItem>,
    #[serde(default)]
    pub video_item: Option<VideoItem>,
    #[serde(default)]
    pub voice_item: Option<VoiceItem>,
    /// 其余未识别字段（forward 兼容 + 协议 spike 线索）
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TextItem {
    pub text: Option<String>,
}

/// iLink CDN 媒体引用（官方 openclaw-weixin 协议 docs/protocol_zh_CN.md）
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CdnMedia {
    #[serde(rename = "encrypt_query_param")]
    pub encrypt_query_param: Option<String>,
    #[serde(rename = "aes_key")]
    pub aes_key: Option<String>,
    /// 存在时优先使用（完整下载 URL）
    #[serde(rename = "full_url")]
    pub full_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ImageItem {
    /// 入站 AES 密钥：32 位十六进制字符串（优先于 media.aes_key）
    pub aeskey: Option<String>,
    pub mid_size: Option<u64>,
    pub media: Option<CdnMedia>,
    #[serde(rename = "thumb_media")]
    pub thumb_media: Option<CdnMedia>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct FileItem {
    pub file_name: Option<String>,
    /// 明文字节数（十进制字符串）
    pub len: Option<String>,
    pub media: Option<CdnMedia>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct VideoItem {
    pub video_size: Option<u64>,
    pub media: Option<CdnMedia>,
    #[serde(rename = "thumb_media")]
    pub thumb_media: Option<CdnMedia>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct VoiceItem {
    /// 存在时为语音转写文本
    pub text: Option<String>,
    pub media: Option<CdnMedia>,
}

/// 官方 CDN 默认地址（服务端返回的 full_url 优先）
pub const DEFAULT_CDN_BASE: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

/// 一个已解析出下载方式的媒体 item
#[derive(Debug, Clone)]
pub struct ResolvedMedia {
    pub kind: crate::plugins::im_core::AttachKind,
    pub url: String,
    /// 16 字节 AES-128 密钥；None = 内容未加密（或无密钥，明文直存）
    pub aes_key: Option<Vec<u8>>,
    pub filename: Option<String>,
}

pub fn random_wechat_uin() -> String {
    let uint32: u32 = rand::thread_rng().gen();
    B64.encode(uint32.to_string().as_bytes())
}

pub fn is_api_error(ret: Option<i64>, errcode: Option<i64>) -> bool {
    matches!(ret, Some(r) if r != 0) || matches!(errcode, Some(c) if c != 0)
}

pub fn is_session_expired(ret: Option<i64>, errcode: Option<i64>) -> bool {
    ret == Some(SESSION_EXPIRED) || errcode == Some(SESSION_EXPIRED)
}

pub fn extract_text(msg: &WeixinMessage) -> String {
    msg.item_list
        .iter()
        .filter_map(|item| item.text_item.as_ref().and_then(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 提取非文本 item（图片/文件等）。
pub fn extract_media_items(msg: &WeixinMessage) -> Vec<&MessageItem> {
    msg.item_list
        .iter()
        .filter(|item| item.text_item.is_none())
        .collect()
}

/// 解析一个媒体 item 的下载方式（依据官方 openclaw-weixin 协议）。
///
/// 1. URL：typed 字段（image_item/file_item/video_item/voice_item）的
///    `media.full_url` 优先；缺失时用 `encrypt_query_param` 构造
///    `<CDN>/download?encrypted_query_param=<urlencoded>`；
///    都没有再走旧版广撒网探测（直连 URL 的兼容路径，密钥为 None）。
/// 2. 密钥：图片优先 `image_item.aeskey`（32 位 hex），其次 `media.aes_key`
///    （base64(16字节raw) 或 base64(hex字符串) 两种形态都接受）。
///    无密钥 = 内容按明文处理。
pub fn resolve_media(item: &MessageItem) -> Option<ResolvedMedia> {
    use crate::plugins::im_core::AttachKind;

    // —— 类型判定：typed 字段优先，其次 item_type 编号（2 图 3 语音 4 文件 5 视频）——
    let (kind, media, hex_key, filename) = if let Some(img) = &item.image_item {
        (
            AttachKind::Image,
            img.media.clone(),
            img.aeskey.clone(),
            None,
        )
    } else if let Some(file) = &item.file_item {
        (
            AttachKind::File,
            file.media.clone(),
            None,
            file.file_name.clone(),
        )
    } else if let Some(video) = &item.video_item {
        (AttachKind::Video, video.media.clone(), None, None)
    } else if let Some(voice) = &item.voice_item {
        (AttachKind::Audio, voice.media.clone(), None, None)
    } else {
        match item.item_type {
            Some(2) => (AttachKind::Image, None, None, None),
            Some(3) => (AttachKind::Audio, None, None, None),
            Some(4) => (AttachKind::File, None, None, None),
            Some(5) => (AttachKind::Video, None, None, None),
            // 学生场景绝大多数是拍照错题：未知类型按图片兜底
            _ => (AttachKind::Image, None, None, None),
        }
    };

    // —— URL 解析 ——
    let url = if let Some(m) = &media {
        if let Some(full) = m
            .full_url
            .as_deref()
            .filter(|s| s.starts_with("http"))
        {
            Some(full.to_string())
        } else {
            m.encrypt_query_param
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|q| {
                    format!(
                        "{}/download?encrypted_query_param={}",
                        DEFAULT_CDN_BASE,
                        urlencoding::encode(q)
                    )
                })
        }
    } else {
        None
    };
    let url = match url {
        Some(u) => u,
        None => return legacy_probe_url(item).map(|u| ResolvedMedia { kind, url: u, aes_key: None, filename }),
    };

    // —— 密钥解析 ——
    let aes_key = decode_media_key(hex_key.as_deref(), media.as_ref().and_then(|m| m.aes_key.as_deref()));

    Some(ResolvedMedia {
        kind,
        url,
        aes_key,
        filename,
    })
}

/// 解码媒体密钥：优先 hex 字符串（image_item.aeskey，32 hex = 16 字节），
/// 其次 base64 形态（raw 16 字节，或 hex 字符串再经 base64 编码）。
fn decode_media_key(hex_key: Option<&str>, b64_key: Option<&str>) -> Option<Vec<u8>> {
    if let Some(hex_str) = hex_key {
        if let Ok(k) = hex::decode(hex_str.trim()) {
            if k.len() == 16 {
                return Some(k);
            }
        }
    }
    let b64 = b64_key?;
    let decoded = B64.decode(b64.trim()).ok()?;
    if decoded.len() == 16 {
        return Some(decoded);
    }
    // base64(hex 字符串) 形态：解出 32 字符 hex 再解一次
    if let Ok(s) = std::str::from_utf8(&decoded) {
        if let Ok(k) = hex::decode(s.trim()) {
            if k.len() == 16 {
                return Some(k);
            }
        }
    }
    None
}

/// 旧版兼容：在未知结构里探测直连 URL（无加密，密钥为 None）。
fn legacy_probe_url(item: &MessageItem) -> Option<String> {
    const URL_KEYS: &[&str] = &["url", "cdn_url", "download_url", "file_url", "pic_url"];
    for key in URL_KEYS {
        if let Some(u) = item
            .extra
            .get(*key)
            .and_then(Value::as_str)
            .filter(|s| s.starts_with("http"))
        {
            return Some(u.to_string());
        }
    }
    const NESTED_KEYS: &[&str] = &[
        "pic_item", "media_item",
    ];
    for key in NESTED_KEYS {
        if let Some(obj) = item.extra.get(*key).and_then(Value::as_object) {
            for key in URL_KEYS {
                if let Some(u) = obj
                    .get(*key)
                    .and_then(Value::as_str)
                    .filter(|s| s.starts_with("http"))
                {
                    return Some(u.to_string());
                }
            }
        }
    }
    None
}

/// AES-128-ECB 解密 + PKCS#7 去填充（微信 CDN 媒体协议规定的形态；
/// ECB 是协议强制的，不是我们选的——媒体均为单块独立的对称加密）。
fn aes_ecb_decrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockDecrypt, KeyInit};
    if key.len() != 16 {
        return Err(format!("AES 密钥长度必须为 16 字节，实际 {}", key.len()));
    }
    if data.is_empty() || data.len() % 16 != 0 {
        return Err(format!("密文长度不是 16 的倍数: {}", data.len()));
    }
    let cipher = aes::Aes128::new(GenericArray::from_slice(key));
    let mut plain = data.to_vec();
    for block in plain.chunks_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    let pad = plain[plain.len() - 1] as usize;
    if pad == 0 || pad > 16 {
        return Err("PKCS#7 填充非法".to_string());
    }
    plain.truncate(plain.len() - pad);
    Ok(plain)
}

/// Split long outbound text into chunks (<= limit runes approx by chars).
pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![];
    }
    if limit == 0 || text.chars().count() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in text.chars() {
        if count >= limit {
            chunks.push(std::mem::take(&mut current));
            count = 0;
        }
        current.push(ch);
        count += 1;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[derive(Clone)]
pub struct IlinkClient {
    http: Client,
}

impl IlinkClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { http }
    }

    fn base(creds: &IlinkCredentials) -> String {
        let b = if creds.base_url.trim().is_empty() {
            DEFAULT_BASE_URL
        } else {
            creds.base_url.trim_end_matches('/')
        };
        b.to_string()
    }

    pub async fn get_bot_qrcode(&self) -> Result<QrcodeResponse, AppError> {
        let url = format!("{}/ilink/bot/get_bot_qrcode?bot_type=3", DEFAULT_BASE_URL);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| AppError::network(format!("get_bot_qrcode 失败: {}", e)))?;
        if !resp.status().is_success() {
            return Err(AppError::network(format!(
                "get_bot_qrcode HTTP {}",
                resp.status()
            )));
        }
        resp.json()
            .await
            .map_err(|e| AppError::validation(format!("解析二维码响应失败: {}", e)))
    }

    pub async fn get_qrcode_status(&self, qrcode: &str) -> Result<QrcodeStatusResponse, AppError> {
        let url = format!(
            "{}/ilink/bot/get_qrcode_status?qrcode={}",
            DEFAULT_BASE_URL,
            urlencoding::encode(qrcode)
        );
        let resp = self
            .http
            .get(&url)
            .header("iLink-App-ClientVersion", "1")
            .timeout(Duration::from_secs(40))
            .send()
            .await
            .map_err(|e| AppError::network(format!("get_qrcode_status 失败: {}", e)))?;
        if !resp.status().is_success() {
            return Err(AppError::network(format!(
                "get_qrcode_status HTTP {}",
                resp.status()
            )));
        }
        resp.json()
            .await
            .map_err(|e| AppError::validation(format!("解析扫码状态失败: {}", e)))
    }

    fn auth_headers(token: &str) -> Result<reqwest::header::HeaderMap, AppError> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            reqwest::header::HeaderName::from_bytes(b"AuthorizationType")
                .map_err(|e| AppError::validation(format!("AuthorizationType 头无效: {}", e)))?,
            reqwest::header::HeaderValue::from_static("ilink_bot_token"),
        );
        let auth = format!("Bearer {}", token);
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&auth)
                .map_err(|e| AppError::validation(format!("Authorization 无效: {}", e)))?,
        );
        headers.insert(
            reqwest::header::HeaderName::from_static("x-wechat-uin"),
            reqwest::header::HeaderValue::from_str(&random_wechat_uin())
                .map_err(|e| AppError::validation(format!("X-WECHAT-UIN 无效: {}", e)))?,
        );
        Ok(headers)
    }

    pub async fn get_updates(
        &self,
        creds: &IlinkCredentials,
        timeout_ms: u64,
    ) -> Result<GetUpdatesResponse, AppError> {
        let url = format!("{}/ilink/bot/getupdates", Self::base(creds));
        let body = json!({
            "get_updates_buf": creds.get_updates_buf,
            "base_info": { "channel_version": CHANNEL_VERSION }
        });
        let headers = Self::auth_headers(&creds.token)?;
        let timeout = Duration::from_millis(timeout_ms.saturating_add(5_000).max(40_000));
        match self
            .http
            .post(&url)
            .headers(headers)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    return Err(AppError::network(format!(
                        "getupdates HTTP {}: {}",
                        status, text
                    )));
                }
                resp.json()
                    .await
                    .map_err(|e| AppError::validation(format!("解析 getupdates 失败: {}", e)))
            }
            Err(e) if is_idle_long_poll_timeout(&e) => {
                // Only a request timeout after connection is established is an
                // empty response. DNS, connection and TLS failures must reach
                // the retry/status path.
                Ok(GetUpdatesResponse {
                    ret: Some(0),
                    errcode: None,
                    errmsg: None,
                    msgs: vec![],
                    get_updates_buf: Some(creds.get_updates_buf.clone()),
                    longpolling_timeout_ms: Some(timeout_ms),
                })
            }
            Err(e) => Err(AppError::network(format!("getupdates 失败: {}", e))),
        }
    }

    /// 下载消息里的媒体；带密钥时按官方协议 AES-128-ECB + PKCS#7 解密
    /// （微信 CDN 上的媒体均为加密存储）。
    pub async fn download_media(
        &self,
        url: &str,
        aes_key: Option<&[u8]>,
    ) -> Result<Vec<u8>, AppError> {
        let resp = self
            .http
            .get(url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| AppError::network(format!("媒体下载失败: {}", e)))?;
        if !resp.status().is_success() {
            return Err(AppError::network(format!(
                "媒体下载 HTTP {}",
                resp.status()
            )));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AppError::network(format!("媒体读取失败: {}", e)))?;
        let data = bytes.to_vec();
        match aes_key {
            None => Ok(data),
            Some(key) => aes_ecb_decrypt(key, &data)
                .map_err(|e| AppError::network(format!("媒体解密失败: {}", e))),
        }
    }

    pub async fn send_text(
        &self,
        creds: &IlinkCredentials,
        to_user_id: &str,
        context_token: &str,
        text: &str,
    ) -> Result<Value, AppError> {
        let url = format!("{}/ilink/bot/sendmessage", Self::base(creds));
        let client_id = format!(
            "dstu-{}-{}",
            chrono::Utc::now().timestamp_millis(),
            &uuid::Uuid::new_v4().to_string()[..8]
        );
        let body = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to_user_id,
                "client_id": client_id,
                "message_type": 2,
                "message_state": 2,
                "context_token": context_token,
                "item_list": [{
                    "type": 1,
                    "text_item": { "text": text }
                }]
            },
            "base_info": { "channel_version": CHANNEL_VERSION }
        });
        let headers = Self::auth_headers(&creds.token)?;
        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::network(format!("sendmessage 失败: {}", e)))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::network(format!(
                "sendmessage HTTP {}: {}",
                status, text
            )));
        }
        let value: Value = resp
            .json()
            .await
            .map_err(|e| AppError::validation(format!("解析 sendmessage 失败: {}", e)))?;
        let ret = value.get("ret").and_then(|v| v.as_i64());
        let errcode = value.get("errcode").and_then(|v| v.as_i64());
        if is_session_expired(ret, errcode) {
            return Err(AppError::configuration("session expired (-14)"));
        }
        if is_api_error(ret, errcode) {
            return Err(AppError::network(format!(
                "sendmessage 业务错误: {}",
                value
            )));
        }
        Ok(value)
    }
}

fn is_idle_long_poll_timeout(error: &reqwest::Error) -> bool {
    error.is_timeout() && !error.is_connect()
}

impl Default for IlinkClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wechat_uin_is_base64_of_decimal() {
        let uin = random_wechat_uin();
        let decoded = B64.decode(&uin).expect("valid base64");
        let s = String::from_utf8(decoded).expect("utf8");
        s.parse::<u32>().expect("decimal u32");
    }

    #[test]
    fn chunk_text_splits() {
        let chunks = chunk_text("abcdefghij", 3);
        assert_eq!(chunks, vec!["abc", "def", "ghi", "j"]);
    }

    #[test]
    fn session_expired_detection() {
        assert!(is_session_expired(Some(-14), None));
        assert!(is_session_expired(None, Some(-14)));
        assert!(!is_session_expired(Some(0), None));
    }

    #[tokio::test]
    async fn connection_failure_is_not_an_empty_success() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/ilink/bot/getupdates")
            .with_status(503)
            .with_body("temporarily unavailable")
            .create_async()
            .await;
        let client = IlinkClient {
            http: Client::builder().no_proxy().build().expect("test client"),
        };
        let creds = IlinkCredentials {
            token: "test-token".into(),
            base_url: server.url(),
            get_updates_buf: "existing-cursor".into(),
            ..Default::default()
        };

        let result = client.get_updates(&creds, 1).await;
        assert!(
            result.is_err(),
            "HTTP failure must enter backoff, not look online"
        );
        assert_eq!(creds.get_updates_buf, "existing-cursor");
        mock.assert_async().await;
    }

    #[test]
    fn outbound_chunks_preserve_unicode_and_order() {
        let text = "微信入口🧑‍💻\n继续学习";
        let chunks = chunk_text(text, 3);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 3));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn unknown_media_items_are_captured_not_dropped() {
        let raw = json!({
            "seq": 7,
            "from_user_id": "peer-1",
            "item_list": [
                { "type": 1, "text_item": { "text": "帮我看看这道题" } },
                { "type": 2, "image_item": { "media": { "full_url": "https://cdn.example.com/pic.jpg", "aes_key": "AAAA" } } },
                { "type": 4, "file_item": { "file_name": "作业.pdf", "media": { "encrypt_query_param": "abc123" } } }
            ]
        });
        let msg: WeixinMessage = serde_json::from_value(raw).expect("parse");
        assert_eq!(msg.item_list.len(), 3, "flatten 必须保留未知 item");
        let media = extract_media_items(&msg);
        assert_eq!(media.len(), 2);
        let r1 = resolve_media(media[0]).expect("image_item full_url");
        assert_eq!(r1.url, "https://cdn.example.com/pic.jpg");
        let r2 = resolve_media(media[1]).expect("file_item encrypt_query_param");
        assert!(r2.url.contains("/download?encrypted_query_param=abc123"), "构造 CDN 下载地址");
        assert_eq!(r2.filename.as_deref(), Some("作业.pdf"));
    }

    #[test]
    fn resolve_media_prefers_full_url_and_hex_aeskey() {
        let item: MessageItem = serde_json::from_value(json!({
            "type": 2,
            "image_item": {
                "aeskey": "000102030405060708090a0b0c0d0e0f",
                "media": { "encrypt_query_param": "q=", "full_url": "https://cdn.example.com/x" }
            }
        }))
        .expect("parse");
        let r = resolve_media(&item).expect("resolved");
        assert_eq!(r.url, "https://cdn.example.com/x", "full_url 优先");
        assert_eq!(
            r.aes_key.as_deref(),
            Some(&[0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f][..]),
            "32 位 hex 密钥解码为 16 字节"
        );
    }

    #[test]
    fn resolve_media_constructs_cdn_url_when_no_full_url() {
        let item: MessageItem = serde_json::from_value(json!({
            "type": 2,
            "image_item": { "media": { "encrypt_query_param": "a b&c=1" } }
        }))
        .expect("parse");
        let r = resolve_media(&item).expect("resolved");
        assert!(r.url.starts_with("https://novac2c.cdn.weixin.qq.com/c2c/download?encrypted_query_param="));
        assert!(r.url.contains("a%20b%26c%3D1"), "query 参数已 URL 编码");
        assert!(r.aes_key.is_none(), "无密钥 = 明文");
    }

    #[test]
    fn decode_media_key_accepts_base64_raw_and_base64_hex() {
        let item: MessageItem = serde_json::from_value(json!({
            "type": 2,
            "image_item": { "media": { "encrypt_query_param": "q", "aes_key": "AAAAAAAAAAAAAAAAAAAAAA==" } }
        }))
        .expect("parse");
        let r = resolve_media(&item).expect("resolved");
        assert_eq!(
            r.aes_key.as_deref(),
            Some(&[0u8; 16][..]),
            "base64(16 字节 raw) 形态"
        );
        // base64(hex 字符串) 形态
        let hex_b64 = B64.encode("deadbeefdeadbeefdeadbeefdeadbeef");
        let item2: MessageItem = serde_json::from_value(json!({
            "type": 2,
            "image_item": { "media": { "encrypt_query_param": "q", "aes_key": hex_b64 } }
        }))
        .expect("parse");
        let r2 = resolve_media(&item2).expect("resolved");
        assert_eq!(
            r2.aes_key.as_deref(),
            Some(&[0xde, 0xad, 0xbe, 0xef, 0xde, 0xad, 0xbe, 0xef, 0xde, 0xad, 0xbe, 0xef, 0xde, 0xad, 0xbe, 0xef][..]),
            "base64(hex 字符串) 形态"
        );
    }

    #[test]
    fn aes_ecb_decrypt_roundtrip_with_pkcs7() {
        // 用已知向量验证：加密 16 字节 0x00 + PKCS#7 满 16 字节填充 = 32 字节密文
        // 这里只验证解密行为：全零密钥解全零密文（无填充）应得全零明文
        let key = [0u8; 16];
        // 手工构造：对 32 字节明文（16 个 0x01 + 16 个 0x02）……直接用加解密对称性验证
        use aes::cipher::generic_array::GenericArray;
        use aes::cipher::{BlockEncrypt, KeyInit};
        let cipher = aes::Aes128::new(GenericArray::from_slice(&key));
        let mut plain = vec![1u8; 16];
        plain.push(2);
        let pad_len = 16 - (plain.len() % 16);
        plain.extend(std::iter::repeat(pad_len as u8).take(pad_len));
        let mut enc = plain.clone();
        for block in enc.chunks_mut(16) {
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
        }
        let dec = aes_ecb_decrypt(&key, &enc).expect("decrypt");
        assert_eq!(dec, vec![1u8; 16], "PKCS#7 去填充后还原明文前缀");
    }

    #[test]
    fn legacy_direct_url_still_works_without_key() {
        let direct: MessageItem = serde_json::from_value(json!({
            "type": 5, "url": "https://x.example.com/a.png"
        }))
        .expect("parse");
        let r = resolve_media(&direct).expect("legacy direct URL");
        assert_eq!(r.url, "https://x.example.com/a.png");
        assert!(r.aes_key.is_none());
        let none: MessageItem =
            serde_json::from_value(json!({ "type": 6, "some_blob": "opaque" })).expect("parse");
        assert!(resolve_media(&none).is_none(), "无 URL 字段返回 None");
        let not_http: MessageItem =
            serde_json::from_value(json!({ "type": 7, "url": "wxfile://local" })).expect("parse");
        assert!(resolve_media(&not_http).is_none(), "非 http URL 不采纳");
    }
}
