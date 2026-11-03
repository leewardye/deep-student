//! 飞书开放平台 REST 客户端（feishu_bot::client）。
//!
//! 覆盖四组稳定公开 API：
//! 1. `POST /auth/v3/tenant_access_token/internal` —— 凭证换取与缓存（有效期 2h，提前 5min 刷新）
//! 2. `GET  /im/v1/messages/{message_id}/resources/{file_key}` —— 收到的图片/文件下载
//! 3. `POST /im/v1/images` —— 出站图片上传（image_type=message）
//! 4. `POST /im/v1/messages?receive_id_type=chat_id` —— 发送 text / image 消息
//!
//! 入站事件订阅不走 REST：见 `bridge.rs`（官方 Node SDK WS 长连接 sidecar）。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::models::AppError;

pub const DEFAULT_BASE_URL: &str = "https://open.feishu.cn/open-apis";

/// 自建应用凭证（secure store 存储，键 plugins::feishu_bot::CREDENTIALS_KEY）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeishuCredentials {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
}

impl FeishuCredentials {
    pub fn is_complete(&self) -> bool {
        !self.app_id.trim().is_empty() && !self.app_secret.trim().is_empty()
    }
}

#[derive(Debug, Clone)]
struct TokenCache {
    token: String,
    /// 过期时刻（Instant 基准）
    expires_at: Instant,
}

#[derive(Clone)]
pub struct FeishuClient {
    http: reqwest::Client,
    base_url: String,
    token_cache: std::sync::Arc<Mutex<Option<TokenCache>>>,
}

#[derive(Debug, Deserialize)]
struct TokenResp {
    code: Option<i64>,
    msg: Option<String>,
    tenant_access_token: Option<String>,
    /// 有效期（秒），官方为 2h
    expire: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SendMessageBody<'a> {
    receive_id: &'a str,
    msg_type: &'a str,
    /// JSON 字符串：text → {"text":"..."}，image → {"image_key":"..."}
    content: String,
}

impl FeishuClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            base_url: DEFAULT_BASE_URL.to_string(),
            token_cache: std::sync::Arc::new(Mutex::new(None)),
        }
    }

    /// tenant_access_token：带缓存，过期前 5 分钟刷新；每次都传当前凭证，
    /// 凭证被更换后旧缓存自动失效（app_id 变更时调用方会清缓存）。
    pub async fn tenant_access_token(&self, creds: &FeishuCredentials) -> Result<String, AppError> {
        if let Ok(g) = self.token_cache.lock() {
            if let Some(cached) = g.as_ref() {
                if Instant::now() < cached.expires_at {
                    return Ok(cached.token.clone());
                }
            }
        }
        let url = format!("{}/auth/v3/tenant_access_token/internal", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "app_id": creds.app_id,
                "app_secret": creds.app_secret,
            }))
            .send()
            .await
            .map_err(|e| AppError::network(format!("获取 tenant_access_token 失败: {}", e)))?;
        let body: TokenResp = resp
            .json()
            .await
            .map_err(|e| AppError::network(format!("解析 token 响应失败: {}", e)))?;
        if body.code.unwrap_or(-1) != 0 {
            return Err(AppError::configuration(format!(
                "飞书凭证无效 (code={:?}): {}",
                body.code,
                body.msg.unwrap_or_default()
            )));
        }
        let token = body
            .tenant_access_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| AppError::configuration("飞书响应缺少 tenant_access_token"))?;
        let expire_secs = body.expire.unwrap_or(7200).saturating_sub(300); // 提前 5 分钟
        if let Ok(mut g) = self.token_cache.lock() {
            *g = Some(TokenCache {
                token: token.clone(),
                expires_at: Instant::now() + Duration::from_secs(expire_secs),
            });
        }
        Ok(token)
    }

    pub fn clear_token_cache(&self) {
        if let Ok(mut g) = self.token_cache.lock() {
            *g = None;
        }
    }

    /// 下载收到的媒体：type = "image" | "file"
    pub async fn download_media(
        &self,
        creds: &FeishuCredentials,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<Vec<u8>, AppError> {
        let token = self.tenant_access_token(creds).await?;
        let url = format!(
            "{}/im/v1/messages/{}/resources/{}?type={}",
            self.base_url, message_id, file_key, resource_type
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| AppError::network(format!("下载媒体失败: {}", e)))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::network(format!(
                "下载媒体 HTTP {}: {}",
                status,
                text.chars().take(200).collect::<String>()
            )));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AppError::network(format!("读取媒体内容失败: {}", e)))?;
        Ok(bytes.to_vec())
    }

    /// 上传出站图片，返回 image_key（multipart 字段名固定为 image）
    pub async fn upload_image(
        &self,
        creds: &FeishuCredentials,
        path: &std::path::Path,
    ) -> Result<String, AppError> {
        let token = self.tenant_access_token(creds).await?;
        let url = format!("{}/im/v1/images?image_type=message", self.base_url);
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| AppError::internal(format!("读取图片失败: {}", e)))?;
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name("image.png")
            .mime_str("image/png")
            .map_err(|e| AppError::internal(format!("构造图片分片失败: {}", e)))?;
        let form = reqwest::multipart::Form::new().part("image", part);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await
            .map_err(|e| AppError::network(format!("上传图片失败: {}", e)))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AppError::network(format!("解析上传图片响应失败: {}", e)))?;
        ensure_ok(&body)?;
        body.pointer("/data/image_key")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| AppError::network("飞书响应缺少 data.image_key"))
    }

    /// 发送文本（自动 JSON 转义）
    pub async fn send_text(
        &self,
        creds: &FeishuCredentials,
        chat_id: &str,
        text: &str,
    ) -> Result<(), AppError> {
        let content = serde_json::json!({ "text": text }).to_string();
        self.send_message(creds, chat_id, "text", &content).await
    }

    /// 发送图片（先上传得 image_key）
    pub async fn send_image(
        &self,
        creds: &FeishuCredentials,
        chat_id: &str,
        image_path: &std::path::Path,
    ) -> Result<(), AppError> {
        let image_key = self.upload_image(creds, image_path).await?;
        let content = serde_json::json!({ "image_key": image_key }).to_string();
        self.send_message(creds, chat_id, "image", &content).await
    }

    async fn send_message(
        &self,
        creds: &FeishuCredentials,
        chat_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<(), AppError> {
        let token = self.tenant_access_token(creds).await?;
        let url = format!("{}/im/v1/messages?receive_id_type=chat_id", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&SendMessageBody {
                receive_id: chat_id,
                msg_type,
                content: content.to_string(),
            })
            .send()
            .await
            .map_err(|e| AppError::network(format!("发送消息失败: {}", e)))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AppError::network(format!("解析发送消息响应失败: {}", e)))?;
        ensure_ok(&body)?;
        Ok(())
    }
}

/// 飞书业务错误码统一校验：code != 0 即失败
fn ensure_ok(body: &serde_json::Value) -> Result<(), AppError> {
    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = body
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(AppError::network(format!("飞书 API code={}: {}", code, msg)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_completeness() {
        let c = FeishuCredentials::default();
        assert!(!c.is_complete());
        let c = FeishuCredentials {
            app_id: "cli_x".into(),
            app_secret: "s".into(),
        };
        assert!(c.is_complete());
    }

    #[test]
    fn ensure_ok_rejects_business_error() {
        let ok = serde_json::json!({"code": 0, "msg": "success"});
        assert!(ensure_ok(&ok).is_ok());
        let err = serde_json::json!({"code": 99991663, "msg": "token invalid"});
        assert!(ensure_ok(&err).is_err());
    }
}
