//! 飞书自建应用机器人通道插件（plugins::feishu_bot）。
//!
//! 架构对齐 ilink_bot：入站消息 → 附件入库（im_media）→ 统一消息（im_core）
//! → `task_command::parse_inbound` → `RemoteTaskManager` → ChatV2 headless
//! 通路执行真实任务。差异点：
//! - 入站事件走官方 Node SDK WS 长连接 sidecar（bridge.rs，无需公网 IP）
//! - 图片/文件收发走飞书 REST（client.rs），天然支持媒体
//! - 绑定模型：设置里保存 bound_open_id，首条私聊消息自动绑定（见 blurb）

mod bridge;
mod client;

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;
use tokio_util::sync::CancellationToken;

use crate::chat_v2::task_command::{self, RemoteTaskManager};
use crate::database::Database;
use crate::models::AppError;
use crate::plugins::events;
use crate::plugins::im_chat;
use crate::plugins::im_core::{
    attachment_manifest, chunk_text, AttachKind, ImChannel, InboundAttachment, UnifiedInbound,
};
use crate::plugins::im_media;
use crate::plugins::manager::PluginManager;
use crate::plugins::types::{
    ChannelPlugin, PluginDescriptor, PluginKind, PluginRuntimeCtx, PluginState,
    PluginStatusSnapshot,
};

use client::{FeishuClient, FeishuCredentials};
// 绑定校验与限流器复用 ilink_bot 的公共实现
use crate::plugins::ilink_bot::guard::{is_bound_user, RateLimiter};

pub const PLUGIN_ID: &str = "feishubot";
pub const CREDENTIALS_KEY: &str = "plugin.feishubot.credentials";
pub const SETTINGS_PREFIX: &str = "plugin.feishubot.";

/// 出站文本分片上限（飞书 text 消息上限远大于微信，4k 字符稳妥）
const OUTBOUND_CHUNK_LIMIT: usize = 4000;
/// 远程任务的 system prompt 追加段
const DEFAULT_REMOTE_SYSTEM_APPEND: &str = "你正在通过飞书机器人远程协助用户。回复使用简洁、友善的简体中文纯文本，结论先行、长度适中。你没有本地文件写入/命令执行权限；涉及写操作的请求请告知用户在桌面端确认。用户发来的图片/文件已落盘，路径见消息中的附件清单。";
/// 同时运行的远程任务上限
const MAX_CONCURRENT_TASKS: usize = 3;
/// 每个会话保留的最近入站文本条数（供闲聊意图分类器参考）
const CHAT_HISTORY_PER_PEER: usize = 10;
/// WS 断线重连的最大退避
const MAX_RECONNECT_DELAY_SECS: u64 = 30;

const DESC: PluginDescriptor = PluginDescriptor {
    id: PLUGIN_ID,
    label: "飞书机器人",
    blurb: "接入飞书自建应用（长连接模式，无需公网 IP），可收发文字、图片与文件，由本地 AI 自动处理。使用步骤：① 填入 App ID / App Secret 并保存；② 在飞书开放平台为应用开通机器人、订阅 im.message.receive_v1 事件、申请 im:message 等权限并发布版本；③ 给机器人发一条私聊消息即自动绑定。收不到回复时点「运行诊断」查看原因。",
    kind: PluginKind::Channel,
};

pub struct FeishuBotPlugin {
    client: FeishuClient,
    /// 远程任务注册表（路由/幂等/隔离，与 ilink_bot 同构）
    tasks: Arc<RemoteTaskManager>,
    last_activity: Mutex<Option<String>>,
    /// 闲聊意图分类的最近入站文本（chat_id → 队列，仅内存，重启即清）
    chat_recent: Mutex<std::collections::HashMap<String, std::collections::VecDeque<String>>>,
}

impl FeishuBotPlugin {
    pub fn new() -> Self {
        Self {
            client: FeishuClient::new(),
            tasks: Arc::new(RemoteTaskManager::default()),
            last_activity: Mutex::new(None),
            chat_recent: Mutex::new(std::collections::HashMap::new()),
        }
    }

    // ---- 凭证与设置 ----

    fn load_credentials(db: &Database) -> Result<Option<FeishuCredentials>, AppError> {
        let raw = db
            .get_secret(CREDENTIALS_KEY)
            .map_err(|e| AppError::database(format!("读取凭证失败: {}", e)))?;
        match raw {
            Some(s) if !s.trim().is_empty() => {
                let creds: FeishuCredentials = serde_json::from_str(&s)
                    .map_err(|e| AppError::validation(format!("凭证损坏: {}", e)))?;
                Ok(if creds.is_complete() { Some(creds) } else { None })
            }
            _ => Ok(None),
        }
    }

    fn save_credentials(db: &Database, creds: &FeishuCredentials) -> Result<(), AppError> {
        let raw = serde_json::to_string(creds)
            .map_err(|e| AppError::internal(format!("序列化凭证失败: {}", e)))?;
        db.save_secret(CREDENTIALS_KEY, &raw)
            .map_err(|e| AppError::database(format!("保存凭证失败: {}", e)))
    }

    fn clear_credentials(db: &Database) -> Result<(), AppError> {
        let _ = db.delete_secret(CREDENTIALS_KEY);
        let _ = db.delete_setting(CREDENTIALS_KEY);
        let _ = db.delete_setting(&format!("{}bound_open_id", SETTINGS_PREFIX));
        let _ = db.delete_setting(&format!("{}bound_chat_id", SETTINGS_PREFIX));
        Ok(())
    }

    fn read_setting(db: &Database, key: &str) -> Option<String> {
        db.get_setting(key).ok().flatten()
    }

    fn read_bound_open_id(db: &Database) -> Option<String> {
        Self::read_setting(db, &format!("{}bound_open_id", SETTINGS_PREFIX))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn read_rate_limit(db: &Database) -> usize {
        Self::read_setting(db, &format!("{}rate_limit_per_min", SETTINGS_PREFIX))
            .and_then(|s| s.parse().ok())
            .unwrap_or(10)
    }

    fn set_activity(&self, summary: &str) {
        if let Ok(mut g) = self.last_activity.lock() {
            *g = Some(summary.to_string());
        }
    }

    // ---- 飞书事件归一化（原生事件 → UnifiedInbound） ----

    /// 解析 `im.message.receive_v1` 事件。仅处理 p2p 私聊（群聊 P1 再放开）。
    /// 返回 None 表示事件被跳过（非消息事件/群聊/不支持的类型）。
    fn normalize_event(event: &Value) -> Option<UnifiedInbound> {
        let message = event.get("message")?;
        let message_id = message
            .get("message_id")
            .and_then(|v| v.as_str())?
            .to_string();
        let chat_id = message
            .get("chat_id")
            .and_then(|v| v.as_str())?
            .to_string();
        let chat_type = message.get("chat_type").and_then(|v| v.as_str());
        if chat_type != Some("p2p") {
            return None;
        }
        let peer_id = event
            .pointer("/sender/sender_id/open_id")
            .and_then(|v| v.as_str())?
            .to_string();
        let msg_type = message
            .get("message_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content: Value =
            serde_json::from_str(message.get("content")?.as_str().unwrap_or("{}")).ok()?;
        let sent_at_ms = message
            .get("create_time")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let (text, attachments) = match msg_type {
            "text" => {
                let raw = content.get("text").and_then(|v| v.as_str()).unwrap_or("");
                (Some(strip_mentions(raw)), Vec::new())
            }
            "image" => {
                let key = content.get("image_key").and_then(|v| v.as_str())?;
                (
                    None,
                    vec![InboundAttachment {
                        kind: AttachKind::Image,
                        media_ref: key.to_string(),
                        filename: None,
                        mime: None,
                        size: None,
                    }],
                )
            }
            "file" => {
                let key = content.get("file_key").and_then(|v| v.as_str())?;
                let name = content
                    .get("file_name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                (
                    None,
                    vec![InboundAttachment {
                        kind: AttachKind::File,
                        media_ref: key.to_string(),
                        filename: name,
                        mime: None,
                        size: None,
                    }],
                )
            }
            "audio" | "media" => {
                let key = content
                    .get("file_key")
                    .or_else(|| content.get("image_key"))
                    .and_then(|v| v.as_str())?;
                let kind = if msg_type == "audio" {
                    AttachKind::Audio
                } else {
                    AttachKind::Video
                };
                (
                    None,
                    vec![InboundAttachment {
                        kind,
                        media_ref: key.to_string(),
                        filename: None,
                        mime: None,
                        size: None,
                    }],
                )
            }
            "post" => {
                // 富文本：收集所有 tag=text 的文本段
                let mut buf = String::new();
                if let Some(blocks) = content.get("content").and_then(|v| v.as_array()) {
                    for block in blocks {
                        // 段落是 segment 数组；部分客户端发单对象形式，两种都兼容
                        let segments: Vec<&serde_json::Value> = match block {
                            serde_json::Value::Array(items) => items.iter().collect(),
                            obj @ serde_json::Value::Object(_) => vec![obj],
                            _ => Vec::new(),
                        };
                        for seg in segments.iter().filter_map(|s| s.as_object()) {
                            if seg.get("tag").and_then(|t| t.as_str()) == Some("text") {
                                if let Some(t) = seg.get("text").and_then(|t| t.as_str()) {
                                    buf.push_str(t);
                                }
                            }
                        }
                        buf.push('\n');
                    }
                }
                let title = content
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let text = format!("{}{}", title, buf).trim().to_string();
                (Some(text), Vec::new())
            }
            _ => return None, // share_chat / interactive 等 P0 不处理
        };

        Some(UnifiedInbound {
            channel: ImChannel::Feishu,
            msg_id: message_id,
            peer_id,
            chat_id,
            text,
            attachments,
            sent_at_ms,
        })
    }

    // ---- 出站 ----

    async fn send_reply(
        &self,
        ctx: &PluginRuntimeCtx,
        creds: &FeishuCredentials,
        chat_id: &str,
        text: &str,
    ) -> Result<(), AppError> {
        for chunk in chunk_text(text, OUTBOUND_CHUNK_LIMIT) {
            match self.client.send_text(creds, chat_id, &chunk).await {
                Ok(_) => {
                    let out = format!("已回复: {}", chunk.chars().take(40).collect::<String>());
                    self.set_activity(&out);
                    events::emit_activity(&ctx.app, PLUGIN_ID, "msg_out", &out);
                }
                Err(e) => {
                    tracing::warn!("[feishubot] send failed: {}", e);
                    // 回复失败必须让用户在 UI 上看得见（常见：平台未给机器人
                    // im:message 发送权限 / 版本未发布），否则无从排查
                    let out = format!("回复发送失败: {}", e);
                    self.set_activity(&out);
                    events::emit_activity(&ctx.app, PLUGIN_ID, "error", &out);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// 下载并落盘消息附件；返回回执列表与失败说明（失败不阻断文本主流程）
    async fn ingest_attachments(
        &self,
        ctx: &PluginRuntimeCtx,
        creds: &FeishuCredentials,
        inbound: &UnifiedInbound,
    ) -> (Vec<crate::plugins::im_core::IngestedAttachment>, Vec<String>) {
        let mut ok = Vec::new();
        let mut errors = Vec::new();
        for (i, att) in inbound.attachments.iter().enumerate() {
            let resource_type = match att.kind {
                AttachKind::Image => "image",
                _ => "file",
            };
            let result = self
                .client
                .download_media(creds, &inbound.msg_id, &att.media_ref, resource_type)
                .await;
            match result {
                Ok(bytes) => {
                    let ext_hint = att.filename.as_deref();
                    match im_media::ingest(
                        &ctx.app,
                        ImChannel::Feishu,
                        &inbound.msg_id,
                        i,
                        ext_hint,
                        att.kind,
                        &bytes,
                        &im_media::extra_allowed_extensions(&ctx.database),
                    ) {
                        Ok(mut item) => {
                            im_media::register_vfs(&ctx.app, &mut item);
                            ok.push(item);
                        }
                        Err(e) => errors.push(format!("附件 {} 入库失败: {}", i + 1, e)),
                    }
                }
                Err(e) => errors.push(format!("附件 {} 下载失败: {}", i + 1, e)),
            }
        }
        (ok, errors)
    }

    // ---- 主循环 ----

    async fn run_event_loop(&self, ctx: PluginRuntimeCtx, creds: FeishuCredentials) {
        let manager = ctx.app.state::<PluginManager>();
        manager
            .set_state(PLUGIN_ID, PluginState::Running, None)
            .await;

        let rate = RateLimiter::new(Self::read_rate_limit(&ctx.database));
        let task_slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TASKS));
        let mut reconnect_failures: u32 = 0;

        'reconnect: loop {
            if ctx.cancel.is_cancelled() {
                return;
            }
            let mut proc = match bridge::spawn(&ctx.app, &creds.app_id, &creds.app_secret).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("[feishubot] bridge spawn failed: {}", e);
                    // 诊断区也要可见（不依赖日志文件），UI「运行诊断」直接显示
                    diag_push_line(format!("[spawn-error] {}", e));
                    self.set_activity(&format!("飞书桥启动失败: {}", e));
                    manager
                        .set_state(PLUGIN_ID, PluginState::Error, Some(e.to_string()))
                        .await;
                    // spawn 失败（无 node / npm 安装失败）不无限重试，等待取消
                    tokio::select! {
                        _ = ctx.cancel.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_secs(60)) => continue 'reconnect,
                    }
                }
            };
            reconnect_failures = 0;
            self.set_activity("飞书长连接已建立");

            let mut stderr_dead = false;
            let mut err_line = String::new();
            'event_loop: loop {
                // stderr 行：桥的启动错误（node 版本/SDK 加载失败）都在这里，
                // 不读就会丢失（pipe 塞满还会把子进程卡死）。EOF/错误后置
                // stderr_dead，防止该分支持续就绪把 stdout 事件饿死。
                let event = tokio::select! {
                    _ = ctx.cancel.cancelled() => return,
                    n = async {
                        if stderr_dead {
                            std::future::pending::<std::io::Result<usize>>().await
                        } else {
                            bridge::read_stderr_line(&mut proc.stderr, &mut err_line).await
                        }
                    } => {
                        match n {
                            Ok(0) | Err(_) => stderr_dead = true,
                            _ => {}
                        }
                        continue 'event_loop;
                    }
                    ev = bridge::read_event(&mut proc.stdout) => ev,
                };
                match event {
                    Ok(Some(ev)) => match ev.name.as_str() {
                        "bridge.exit" => {
                            // 子进程退出（stdout EOF）：必须显式重连。此前 None
                            // 被当空行忽略导致忙旋转（v0.12.2 修复）
                            let code = proc.child.try_wait().ok().flatten()
                                .map(|s| s.code().unwrap_or(-1).to_string())
                                .unwrap_or_else(|| "信号终止".to_string());
                            let out = format!("飞书桥进程退出（exit {}）", code);
                            tracing::warn!("[feishubot] {}", out);
                            diag_push_line(format!("[bridge-exit] {}", out));
                            self.set_activity(&out);
                            events::emit_activity(&ctx.app, PLUGIN_ID, "warn", &out);
                            break 'event_loop; // 触发重连
                        }
                        "bridge.error" => {
                            // 桥进程/SDK 错误必须让用户看得见（常见：appId 无效、
                            // 无事件订阅权限、长连接被踢等）
                            DIAG.bridge_errors
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let msg = ev.data.as_str().unwrap_or("未知错误");
                            diag_push_line(format!("[bridge-error] {}", msg));
                            let out = format!("飞书桥错误: {}", msg);
                            tracing::warn!("[feishubot] {}", out);
                            self.set_activity(&out);
                            events::emit_activity(&ctx.app, PLUGIN_ID, "error", &out);
                        }
                        "bridge.log" => {
                            let msg = ev.data.as_str().unwrap_or("").to_string();
                            diag_push_line(format!("[bridge] {}", msg));
                            tracing::info!("[feishubot][bridge] {}", msg);
                        }
                        "im.message.receive_v1" => {
                            DIAG.events_received
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            {
                                if let Ok(mut g) = DIAG.last_event_at_ms.lock() {
                                    *g = Some(now_ms_i64());
                                }
                            }
                            match Self::normalize_event(&ev.data) {
                                Some(inbound) => {
                                    self.handle_inbound(&ctx, &creds, &rate, &task_slots, inbound)
                                        .await;
                                }
                                None => {
                                    // 事件被跳过时说明原因（群聊/不支持类型/解析失败），
                                    // 否则用户无法区分「没收到」和「收到但忽略了」
                                    if let Some(reason) = explain_skipped_event(&ev.data) {
                                        diag_push_line(format!("[skipped] {}", reason));
                                        tracing::info!("[feishubot] {}", reason);
                                        self.set_activity(&reason);
                                        events::emit_activity(
                                            &ctx.app,
                                            PLUGIN_ID,
                                            "warn",
                                            &reason,
                                        );
                                    }
                                }
                            }
                        }
                        _ => {}
                    },
                    Ok(None) => { /* 空行，继续 */ }
                    Err(e) => {
                        tracing::warn!("[feishubot] bridge read error: {}", e);
                        self.set_activity(&format!("连接异常: {}", e));
                        break; // 触发重连
                    }
                }
            }

            reconnect_failures += 1;
            let delay = Duration::from_secs(
                (2u64.saturating_pow(reconnect_failures.min(5))).min(MAX_RECONNECT_DELAY_SECS),
            );
            let out = format!("连接断开，{} 秒后重连（第 {} 次）", delay.as_secs(), reconnect_failures);
            diag_push_line(format!("[reconnect] {}", out));
            self.set_activity(&out);
            events::emit_activity(&ctx.app, PLUGIN_ID, "warn", &out);
            tracing::info!("[feishubot] reconnecting in {:?}...", delay);
            tokio::select! {
                _ = ctx.cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    async fn handle_inbound(
        &self,
        ctx: &PluginRuntimeCtx,
        creds: &FeishuCredentials,
        rate: &RateLimiter,
        task_slots: &Arc<tokio::sync::Semaphore>,
        inbound: UnifiedInbound,
    ) {
        let db = &ctx.database;
        let bound = Self::read_bound_open_id(db);

        // 绑定：首条私聊消息自动绑定（blurb 已说明），之后仅接受绑定者
        let bound = match bound {
            Some(b) => b,
            None => {
                let _ = db.save_setting(
                    &format!("{}bound_open_id", SETTINGS_PREFIX),
                    &inbound.peer_id,
                );
                events::emit_activity(
                    &ctx.app,
                    PLUGIN_ID,
                    "login",
                    &format!("已自动绑定发送者 {}", inbound.peer_id),
                );
                inbound.peer_id.clone()
            }
        };
        if !is_bound_user(&bound, &inbound.peer_id) {
            self.set_activity("已忽略非绑定用户消息");
            events::emit_activity(&ctx.app, PLUGIN_ID, "warn", "非绑定用户消息已忽略");
            return;
        }

        // 记录/刷新绑定者的私聊会话 ID（桌面端主动推送出站寻址用；chat_id 变化时自动跟随）
        let _ = db.save_setting(
            &format!("{}bound_chat_id", SETTINGS_PREFIX),
            &inbound.chat_id,
        );

        // 附件入库（在限流前执行：重投会命中去重，代价低）
        let (ingested, attach_errors) = self.ingest_attachments(ctx, creds, &inbound).await;
        if !attach_errors.is_empty() {
            let note = attach_errors.join("；");
            let _ = self
                .client
                .send_text(creds, &inbound.chat_id, &format!("⚠️ {}", note))
                .await;
        }

        // 组装文本：用户文本 + 附件清单；纯图片时给默认指令
        let user_text = inbound
            .text
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| {
                if !ingested.is_empty() {
                    "请处理刚发来的附件。".to_string()
                } else {
                    String::new()
                }
            });
        let full_text = format!("{}{}", user_text, attachment_manifest(&ingested));
        if full_text.trim().is_empty() {
            return;
        }

        // 限流（控制类消息不限）
        let is_control_message = matches!(
            task_command::parse_inbound(&full_text),
            task_command::ParsedInbound::Help
                | task_command::ParsedInbound::Command(
                    task_command::TaskCommand::Stop { .. }
                        | task_command::TaskCommand::Inspect { .. }
                        | task_command::TaskCommand::Approve { .. }
                )
        );
        if !is_control_message && !rate.check_and_record(&inbound.peer_id) {
            let _ = self
                .client
                .send_text(creds, &inbound.chat_id, "你发送得太快了，请稍后再试。")
                .await;
            return;
        }

        let summary = format!(
            "收到消息: {}",
            user_text.chars().take(40).collect::<String>()
        );
        self.set_activity(&summary);
        events::emit_activity(&ctx.app, PLUGIN_ID, "msg_in", &summary);

        // —— 任务路由（与 ilink_bot 同构）——
        let binding = task_command::RemoteBinding {
            account: creds.app_id.clone(),
            device_id: bound.clone(),
            channel: task_command::CHANNEL_FEISHU.to_string(),
            thread_id: inbound.chat_id.clone(),
            ..Default::default()
        };
        let dedup_key = if inbound.msg_id.is_empty() {
            String::new()
        } else {
            format!("f{}", inbound.msg_id)
        };

        // —— 闲聊意图判定（仅普通消息；命令/帮助不分类）——
        let is_plain_message = matches!(
            task_command::parse_inbound(&full_text),
            task_command::ParsedInbound::Message(_)
        );
        let chat = if is_plain_message {
            let recent: Vec<String> = self
                .chat_recent
                .lock()
                .ok()
                .and_then(|m| m.get(&inbound.chat_id).cloned())
                .unwrap_or_default()
                .into_iter()
                .collect();
            match im_chat::is_chat_intent(&ctx.app, &recent, &full_text).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("[feishubot] 意图分类失败，按任务处理: {}", e);
                    false
                }
            }
        } else {
            false
        };
        if let Ok(mut m) = self.chat_recent.lock() {
            let dq = m.entry(inbound.chat_id.clone()).or_default();
            dq.push_back(full_text.clone());
            while dq.len() > CHAT_HISTORY_PER_PEER {
                dq.pop_front();
            }
        }

        let outcome = self.tasks.dispatch(task_command::InboundRemote {
            binding,
            message_id: dedup_key,
            text: full_text.clone(),
            chat,
        });

        match outcome {
            task_command::DispatchOutcome::IgnoredDuplicate => {}
            task_command::DispatchOutcome::Reply(reply) => {
                let _ = self.send_reply(ctx, creds, &inbound.chat_id, &reply).await;
            }
            task_command::DispatchOutcome::StopAck { reply, session_id } => {
                if let Some(sid) = session_id {
                    let cancelled =
                        task_command::cancel_session_stream_with_retry(&ctx.app, &sid).await;
                    tracing::info!(
                        "[feishubot] stop cancel session={} cancelled={}",
                        sid,
                        cancelled
                    );
                }
                let _ = self.send_reply(ctx, creds, &inbound.chat_id, &reply).await;
            }
            task_command::DispatchOutcome::Launch { ack, plan } => {
                // 闲聊轮 ack 为空：静默启动，回复即结果
                if !ack.is_empty() {
                    if let Err(e) = self
                        .send_reply(ctx, creds, &inbound.chat_id, &ack)
                        .await
                    {
                        tracing::warn!("[feishubot] send ack failed: {}", e);
                    }
                }
                let model_id = Self::read_setting(
                    db,
                    &format!("{}model_config_id", SETTINGS_PREFIX),
                );
                let system_prompt =
                    Self::read_setting(db, &format!("{}system_prompt", SETTINGS_PREFIX));
                let system_append = if plan.chat {
                    Some(im_chat::CHAT_SYSTEM_APPEND.to_string())
                } else {
                    Some(
                        system_prompt
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(DEFAULT_REMOTE_SYSTEM_APPEND)
                            .to_string(),
                    )
                };
                let permit = match task_slots.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        if let Some(text) = self.tasks.finish_turn_at_generation(
                            &plan.task_id,
                            Some(plan.binding.generation),
                            task_command::RemoteTaskStatus::Failed,
                            "",
                            Some("同时运行的任务已达上限，请稍后再试"),
                        ) {
                            let _ = self
                                .send_reply(ctx, creds, &inbound.chat_id, &text)
                                .await;
                        }
                        return;
                    }
                };
                let app = ctx.app.clone();
                let tasks = Arc::clone(&self.tasks);
                let client = self.client.clone();
                let creds_c = creds.clone();
                let chat_id = inbound.chat_id.clone();
                let cancellation = ctx.cancel.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    run_remote_task(
                        app,
                        tasks,
                        client,
                        creds_c,
                        chat_id,
                        plan,
                        model_id,
                        system_append,
                        cancellation,
                    )
                    .await;
                });
            }
        }
    }
}

/// 归纳 `im.message.receive_v1` 事件被跳过的原因（供活动日志展示）。
/// 返回 None 表示不是消息事件（如卡片回调），不打扰用户。
fn explain_skipped_event(event: &Value) -> Option<String> {
    let message = event.get("message")?;
    let chat_type = message
        .get("chat_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if chat_type == "group" {
        return Some("收到群聊消息：当前仅支持私聊机器人，请直接私聊发消息".to_string());
    }
    if event
        .pointer("/sender/sender_id/open_id")
        .and_then(|v| v.as_str())
        .is_none()
    {
        return Some("收到消息事件但缺少发送者信息，已忽略".to_string());
    }
    let msg_type = message
        .get("message_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    Some(format!(
        "收到暂不支持的消息类型「{}」（支持：文字/图片/文件/音频/视频/富文本）",
        msg_type
    ))
}

/// 去掉飞书文本中的 @ 占位符（@_user_1）
fn strip_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("@_user_") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + "@_user_".len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        rest = &after[digits.len()..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// 后台执行远程任务（与 ilink_bot::run_remote_task 同构，仅发送通道不同）
#[allow(clippy::too_many_arguments)]
async fn run_remote_task(
    app: tauri::AppHandle,
    tasks: Arc<RemoteTaskManager>,
    client: FeishuClient,
    creds: FeishuCredentials,
    chat_id: String,
    plan: task_command::PlannedTurn,
    model_id: Option<String>,
    system_append: Option<String>,
    cancellation: CancellationToken,
) {
    if cancellation.is_cancelled() {
        return;
    }
    let session_id = match plan.existing_session_id.clone() {
        Some(s) => Some(s),
        None => match task_command::create_remote_task_session(&app, &plan) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("[feishubot] create remote task session failed: {}", e);
                None
            }
        },
    };
    let Some(session_id) = session_id else {
        if let Some(text) = tasks.finish_turn_at_generation(
            &plan.task_id,
            Some(plan.binding.generation),
            task_command::RemoteTaskStatus::Failed,
            "",
            Some("ChatV2 未就绪，任务启动失败"),
        ) {
            let _ = send_chunks_out(&client, &creds, &chat_id, &text).await;
        }
        return;
    };
    if !tasks.attach_session_at_generation(&plan.task_id, &session_id, plan.binding.generation) {
        tracing::info!(
            "[feishubot] task {} stopped before session attach, abort launch",
            plan.task_id
        );
        return;
    }
    let mut turn =
        task_command::build_session_turn(&plan, session_id.clone(), model_id, system_append);
    turn.cancellation_token = Some(cancellation.clone());
    let result = crate::chat_v2::headless::run_headless_agent_turn(&app, turn).await;
    let (status, summary, error) = match result {
        Ok(outcome) => (
            task_command::RemoteTaskStatus::Completed,
            outcome.content,
            None,
        ),
        Err(m) if m.contains("timed out") => (
            task_command::RemoteTaskStatus::Timeout,
            String::new(),
            Some(m),
        ),
        Err(m) if m.contains("cancelled") => (
            task_command::RemoteTaskStatus::Cancelled,
            String::new(),
            Some(m),
        ),
        Err(m) => (
            task_command::RemoteTaskStatus::Failed,
            String::new(),
            Some(m),
        ),
    };
    if let Some(text) = tasks.finish_turn_at_generation(
        &plan.task_id,
        Some(plan.binding.generation),
        status,
        &summary,
        error.as_deref(),
    ) {
        if cancellation.is_cancelled() {
            return;
        }
        match send_chunks_out(&client, &creds, &chat_id, &text).await {
            Ok(_) => {
                events::emit_activity(
                    &app,
                    PLUGIN_ID,
                    "msg_out",
                    &format!("任务 #{} 结果已发送", plan.task_id),
                );
            }
            Err(e) => {
                tracing::warn!("[feishubot] send task result failed: {}", e);
                events::emit_activity(&app, PLUGIN_ID, "warn", &format!("任务结果发送失败: {}", e));
            }
        }
    }
}

// 模块级辅助（避免与 trait 方法重名）
async fn send_chunks_out(
    client: &FeishuClient,
    creds: &FeishuCredentials,
    chat_id: &str,
    text: &str,
) -> Result<(), AppError> {
    for chunk in chunk_text(text, OUTBOUND_CHUNK_LIMIT) {
        client.send_text(creds, chat_id, &chunk).await?;
    }
    Ok(())
}

#[async_trait]
impl ChannelPlugin for FeishuBotPlugin {
    fn descriptor(&self) -> &PluginDescriptor {
        &DESC
    }

    fn is_configured(&self, config: &Value) -> bool {
        config
            .get("configured")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    async fn is_bound(&self, database: &Database) -> bool {
        // 飞书语义：凭证就绪即可启用/自启（真正的发送者绑定在首条消息时自动锁定，
        // 见 handle_inbound）。若这里要求 bound_open_id，插件将永远无法启动收首条消息。
        Self::load_credentials(database)
            .ok()
            .flatten()
            .map(|c| c.is_complete())
            .unwrap_or(false)
    }

    async fn run(&self, ctx: PluginRuntimeCtx) -> Result<(), AppError> {
        let creds = Self::load_credentials(&ctx.database)?.ok_or_else(|| {
            AppError::configuration("飞书未配置：请先在设置中填写 App ID 与 App Secret")
        })?;
        // 启动即验证凭证，凭证错误快速失败并展示
        if let Err(e) = self.client.tenant_access_token(&creds).await {
            let manager = ctx.app.state::<PluginManager>();
            manager
                .set_state(PLUGIN_ID, PluginState::Error, Some(e.to_string()))
                .await;
            return Err(e);
        }
        self.run_event_loop(ctx, creds).await;
        Ok(())
    }

    async fn status(&self, database: &Database) -> PluginStatusSnapshot {
        let creds = Self::load_credentials(database).ok().flatten();
        let enabled = Self::read_setting(
            database,
            &format!("{}enabled", SETTINGS_PREFIX),
        )
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
        let config = self.get_public_config(database).await.unwrap_or_default();
        PluginStatusSnapshot {
            state: PluginState::Stopped,
            enabled,
            configured: self.is_configured(&config),
            // 与 is_bound 一致：凭证就绪 = 可运行
            bound: creds.as_ref().map(|c| c.is_complete()).unwrap_or(false),
            login_status: None,
            account_id: creds.as_ref().map(|c| c.app_id.clone()),
            user_id: Self::read_bound_open_id(database),
            last_error: None,
            last_activity: self.last_activity.lock().ok().and_then(|g| g.clone()),
            qrcode_png_base64: None,
            qrcode_status: None,
        }
    }

    async fn get_public_config(&self, database: &Database) -> Result<Value, AppError> {
        let creds = Self::load_credentials(database).ok().flatten();
        let configured = creds.as_ref().map(|c| c.is_complete()).unwrap_or(false);
        Ok(json!({
            "enabled": Self::read_setting(database, &format!("{}enabled", SETTINGS_PREFIX))
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            "rateLimitPerMin": Self::read_rate_limit(database),
            "modelConfigId": Self::read_setting(database, &format!("{}model_config_id", SETTINGS_PREFIX)).unwrap_or_default(),
            "systemPrompt": Self::read_setting(database, &format!("{}system_prompt", SETTINGS_PREFIX)).unwrap_or_default(),
            "extraExt": Self::read_setting(database, crate::plugins::im_media::EXTRA_EXT_SETTING_KEY).unwrap_or_default(),
            "appId": creds.as_ref().map(|c| c.app_id.clone()).unwrap_or_default(),
            "hasSecret": creds.as_ref().map(|c| !c.app_secret.is_empty()).unwrap_or(false),
            "configured": configured,
            "bound": configured,
            "boundOpenId": Self::read_bound_open_id(database).unwrap_or_default(),
        }))
    }

    async fn set_config(&self, database: &Database, patch: Value) -> Result<(), AppError> {
        if let Some(v) = patch.get("enabled").and_then(|x| x.as_bool()) {
            database
                .save_setting(
                    &format!("{}enabled", SETTINGS_PREFIX),
                    if v { "true" } else { "false" },
                )
                .map_err(|e| AppError::database(e.to_string()))?;
        }
        if let Some(n) = patch.get("rateLimitPerMin").and_then(|x| x.as_u64()) {
            database
                .save_setting(
                    &format!("{}rate_limit_per_min", SETTINGS_PREFIX),
                    &n.to_string(),
                )
                .map_err(|e| AppError::database(e.to_string()))?;
        }
        if let Some(s) = patch.get("modelConfigId").and_then(|x| x.as_str()) {
            database
                .save_setting(&format!("{}model_config_id", SETTINGS_PREFIX), s)
                .map_err(|e| AppError::database(e.to_string()))?;
        }
        if let Some(s) = patch.get("systemPrompt").and_then(|x| x.as_str()) {
            database
                .save_setting(&format!("{}system_prompt", SETTINGS_PREFIX), s)
                .map_err(|e| AppError::database(e.to_string()))?;
        }
        if let Some(s) = patch.get("extraExt").and_then(|x| x.as_str()) {
            database
                .save_setting(crate::plugins::im_media::EXTRA_EXT_SETTING_KEY, s)
                .map_err(|e| AppError::database(e.to_string()))?;
        }
        // 凭证写入：appId / appSecret 均可经设置页更新（secure store）。
        // appId 变更时清空 token 缓存与绑定关系（避免串号）。
        let mut app_id_changed = false;
        let mut creds = Self::load_credentials(database)?.unwrap_or_default();
        if let Some(s) = patch.get("appId").and_then(|x| x.as_str()) {
            let s = s.trim().to_string();
            if creds.app_id != s {
                app_id_changed = !creds.app_id.is_empty();
                creds.app_id = s;
            }
        }
        if let Some(s) = patch.get("appSecret").and_then(|x| x.as_str()) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                creds.app_secret = s;
            }
        }
        if app_id_changed {
            self.client.clear_token_cache();
            let _ = database.delete_setting(&format!("{}bound_open_id", SETTINGS_PREFIX));
        }
        if creds.is_complete() && (patch.get("appId").is_some() || patch.get("appSecret").is_some())
        {
            Self::save_credentials(database, &creds)?;
        }
        Ok(())
    }

    async fn begin_login(&self, _ctx: PluginRuntimeCtx) -> Result<(), AppError> {
        Err(AppError::validation(
            "飞书无需扫码登录：请在设置中填写 App ID 与 App Secret 后启动插件",
        ))
    }

    async fn cancel_login(&self) -> Result<(), AppError> {
        Ok(())
    }

    async fn logout(&self, database: &Database) -> Result<(), AppError> {
        // 解绑 = 清绑定关系；凭证保留（重新启用无需重填）
        let _ = database.delete_setting(&format!("{}bound_open_id", SETTINGS_PREFIX));
        let _ = database.delete_setting(&format!("{}bound_chat_id", SETTINGS_PREFIX));
        self.set_activity("已解绑");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 桌面端主动推送（对齐 openclaw 官方出站做法：文本分片 + 媒体先上传再发送）
// ---------------------------------------------------------------------------

// ---- 诊断（全局计数与最近桥接日志，进程内共享）----

/// 飞书通道运行期诊断数据。全局静态：命令层与插件实例解耦，避免向下转型。
struct FeishuDiag {
    /// 收到的 im.message.receive_v1 事件总数（无论是否被处理）
    events_received: std::sync::atomic::AtomicU64,
    /// 桥进程错误行数
    bridge_errors: std::sync::atomic::AtomicU64,
    /// 最近一次收到消息事件的本地时间戳（ms）
    last_event_at_ms: Mutex<Option<i64>>,
    /// 最近 30 条桥接诊断行（bridge.log / bridge.error / 重连记录）
    recent_lines: Mutex<std::collections::VecDeque<String>>,
}

static DIAG: std::sync::LazyLock<FeishuDiag> = std::sync::LazyLock::new(|| FeishuDiag {
    events_received: std::sync::atomic::AtomicU64::new(0),
    bridge_errors: std::sync::atomic::AtomicU64::new(0),
    last_event_at_ms: Mutex::new(None),
    recent_lines: Mutex::new(std::collections::VecDeque::with_capacity(32)),
});

fn diag_push_line(line: String) {
    if let Ok(mut q) = DIAG.recent_lines.lock() {
        if q.len() >= 30 {
            q.pop_front();
        }
        q.push_back(line);
    }
}

fn now_ms_i64() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn format_local_ms(ms: i64) -> String {
    // 用 chrono 本地时区格式化（events.rs 已依赖 chrono）
    chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms.max(0) as u64),
    )
    .format("%m-%d %H:%M:%S")
    .to_string()
}

/// 飞书连接诊断：凭证/token/桥进程/事件流一次性体检。
///
/// 结果为结构化 JSON，前端「运行诊断」按钮直接渲染。注意：
/// - 长连接是集群模式，诊断**不会**再开第二条 WS（会分摊走真连接的事件）
/// - token 校验 = 真实调用飞书 REST，能同时验证 appId/secret/网络可达性
/// - 平台侧配置（事件订阅/权限/版本发布）无法经 API 探测，只能由事件计数
///   区分：events_received 持续为 0 而前几项全过 = 平台配置问题
#[tauri::command]
pub async fn feishu_diagnose(app: tauri::AppHandle) -> Result<serde_json::Value, AppError> {
    use std::sync::Arc;
    let db = app.state::<Arc<Database>>();
    let creds = FeishuBotPlugin::load_credentials(&db)?;

    let mut items: Vec<serde_json::Value> = Vec::new();
    let mut all_ok = true;

    // ① 凭证
    match &creds {
        Some(c) if c.is_complete() => items.push(json!({
            "key": "credentials", "ok": true,
            "detail": format!("已配置（App ID: {}）", c.app_id),
        })),
        _ => {
            all_ok = false;
            items.push(json!({
                "key": "credentials", "ok": false,
                "detail": "未配置或缺少 App ID / App Secret",
            }));
        }
    }

    // ② token（真实 REST 校验，顺带验证到 open.feishu.cn 的网络连通性）
    if let Some(creds) = &creds {
        let client = FeishuClient::new();
        match client.tenant_access_token(creds).await {
            Ok(_) => items.push(json!({
                "key": "token", "ok": true,
                "detail": "凭证有效（tenant_access_token 获取成功）",
            })),
            Err(e) => {
                all_ok = false;
                items.push(json!({
                    "key": "token", "ok": false,
                    "detail": format!("凭证校验失败（App Secret 错误或网络不通）: {}", e),
                }));
            }
        }
    }

    // ③ 桥进程依赖（node + SDK 安装）
    let bridge_dir = bridge::bridge_dir(&app)?;
    let sdk_ready = bridge_dir
        .join("node_modules")
        .join("@larksuiteoapi")
        .exists();
    items.push(json!({
        "key": "bridge_env", "ok": sdk_ready,
        "detail": if sdk_ready {
            format!("SDK 已就绪（{}）", bridge_dir.display())
        } else {
            format!("SDK 未安装（{}），插件启动时会自动 npm install", bridge_dir.display())
        },
    }));
    if !sdk_ready {
        all_ok = false;
    }

    // ④ 事件流（收到事件 = 平台配置全部正确、WS 正常推送）
    let events = DIAG
        .events_received
        .load(std::sync::atomic::Ordering::Relaxed);
    let last_at = DIAG.last_event_at_ms.lock().ok().and_then(|g| *g);
    let (events_ok, events_detail) = if events > 0 {
        (
            true,
            format!(
                "已收到 {} 条消息事件，最近一次 {}",
                events,
                last_at.map(format_local_ms).unwrap_or_default()
            ),
        )
    } else {
        (false, "尚未收到任何消息事件".to_string())
    };
    items.push(json!({
        "key": "events", "ok": events_ok, "detail": events_detail,
    }));

    let recent: Vec<String> = DIAG
        .recent_lines
        .lock()
        .map(|q| q.iter().cloned().collect())
        .unwrap_or_default();

    let bound_open_id = FeishuBotPlugin::read_bound_open_id(&db).unwrap_or_default();
    let bound_chat_id = FeishuBotPlugin::read_setting(
        &db,
        &format!("{}bound_chat_id", SETTINGS_PREFIX),
    )
    .unwrap_or_default();

    Ok(json!({
        "all_ok": all_ok && events_ok,
        "items": items,
        "bound_open_id": bound_open_id,
        "bound_chat_id": bound_chat_id,
        "events_received": events,
        "bridge_errors": DIAG.bridge_errors.load(std::sync::atomic::Ordering::Relaxed),
        "recent_lines": recent,
        "platform_checklist": [
            "开发者后台 → 事件与回调 → 订阅方式必须选「使用长连接接收事件」",
            "事件与回调 → 添加事件 im.message.receive_v1（接收消息）",
            "权限管理 → 开通 im:message（发消息）与 im:message.p2p:readonly（读私聊）",
            "版本管理与发布 → 创建版本并发布（权限/事件变更后需发新版本）",
            "机器人能力 → 开启机器人；测试号需把自己加入应用可用范围",
        ],
    }))
}

/// 桌面端向绑定的飞书私聊主动推送消息（文本/图片/文件）。
#[tauri::command]
pub async fn feishu_push_message(
    app: tauri::AppHandle,
    text: Option<String>,
    image_paths: Option<Vec<String>>,
    file_paths: Option<Vec<String>>,
) -> Result<String, AppError> {
    use std::sync::Arc;
    let db = app.state::<Arc<Database>>();
    let creds = FeishuBotPlugin::load_credentials(&db)?.ok_or_else(|| {
        AppError::validation("飞书凭证未配置，请先在设置 → 插件中填写 App ID / App Secret")
    })?;
    let chat_id = FeishuBotPlugin::read_setting(
        &db,
        &format!("{}bound_chat_id", SETTINGS_PREFIX),
    )
    .filter(|s| !s.trim().is_empty())
    .ok_or_else(|| {
        AppError::validation("尚无绑定的飞书会话：请先用绑定账号给机器人发一条消息")
    })?;

    let client = FeishuClient::new();
    let mut sent: Vec<String> = Vec::new();

    if let Some(t) = text.filter(|s| !s.trim().is_empty()) {
        for chunk in chunk_text(&t, OUTBOUND_CHUNK_LIMIT) {
            client.send_text(&creds, &chat_id, &chunk).await?;
        }
        sent.push("文本".to_string());
    }

    for path in image_paths.unwrap_or_default().iter() {
        let p = std::path::Path::new(path);
        client.send_image(&creds, &chat_id, p).await?;
        sent.push(format!(
            "图片 {}",
            p.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        ));
    }

    for path in file_paths.unwrap_or_default().iter() {
        let p = std::path::Path::new(path);
        client.send_file(&creds, &chat_id, p).await?;
        sent.push(format!(
            "文件 {}",
            p.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        ));
    }

    if sent.is_empty() {
        return Err(AppError::validation("没有可发送的内容（文本/图片/文件均为空）"));
    }

    events::emit_activity(
        &app,
        PLUGIN_ID,
        "msg_out",
        &format!("桌面端推送: {}", sent.join("、")),
    );
    Ok(format!("已发送 {}", sent.join("、")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_text_event() {
        let event: Value = serde_json::from_str(
            r#"{
                "sender": {"sender_id": {"open_id": "ou_abc"}},
                "message": {
                    "message_id": "om_1",
                    "chat_id": "oc_1",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"@_user_1 帮我建个任务\"}",
                    "create_time": "1700000000000"
                }
            }"#,
        )
        .unwrap();
        let u = FeishuBotPlugin::normalize_event(&event).unwrap();
        assert_eq!(u.msg_id, "om_1");
        assert_eq!(u.peer_id, "ou_abc");
        assert_eq!(u.text.as_deref(), Some("帮我建个任务"));
        assert!(u.attachments.is_empty());
    }

    #[test]
    fn normalize_image_event() {
        let event: Value = serde_json::from_str(
            r#"{
                "sender": {"sender_id": {"open_id": "ou_abc"}},
                "message": {
                    "message_id": "om_2",
                    "chat_id": "oc_1",
                    "chat_type": "p2p",
                    "message_type": "image",
                    "content": "{\"image_key\":\"img_v2_key\"}",
                    "create_time": "1700000000001"
                }
            }"#,
        )
        .unwrap();
        let u = FeishuBotPlugin::normalize_event(&event).unwrap();
        assert!(u.text.is_none());
        assert_eq!(u.attachments.len(), 1);
        assert_eq!(u.attachments[0].media_ref, "img_v2_key");
        assert_eq!(u.attachments[0].kind, AttachKind::Image);
    }

    #[test]
    fn normalize_skips_group_chat() {
        let event: Value = serde_json::from_str(
            r#"{
                "sender": {"sender_id": {"open_id": "ou_abc"}},
                "message": {
                    "message_id": "om_3",
                    "chat_id": "oc_g",
                    "chat_type": "group",
                    "message_type": "text",
                    "content": "{\"text\":\"hi\"}",
                    "create_time": "1700000000002"
                }
            }"#,
        )
        .unwrap();
        assert!(FeishuBotPlugin::normalize_event(&event).is_none());
    }

    #[test]
    fn normalize_parses_post_rich_text() {
        let event: Value = serde_json::from_str(
            r#"{
                "sender": {"sender_id": {"open_id": "ou_abc"}},
                "message": {
                    "message_id": "om_4",
                    "chat_id": "oc_1",
                    "chat_type": "p2p",
                    "message_type": "post",
                    "content": "{\"title\":\"作业\",\"content\":[[{\"tag\":\"text\",\"text\":\"第一章错题\"}]]}",
                    "create_time": "1700000000003"
                }
            }"#,
        )
        .unwrap();
        let u = FeishuBotPlugin::normalize_event(&event).unwrap();
        assert!(u.text.as_deref().unwrap_or_default().contains("第一章错题"));
    }

    #[test]
    fn strip_mentions_works() {
        assert_eq!(strip_mentions("@_user_1 你好"), "你好");
        assert_eq!(strip_mentions("你好"), "你好");
    }
}
