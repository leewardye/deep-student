//! 飞书 WS 长连接 sidecar（feishu_bot::bridge）。
//!
//! 飞书长连接协议由官方 SDK 封装（握手/ticket/心跳/重连均私有实现，无公开
//! 稳定文档），社区共识是复用官方 SDK 而非手写协议。因此入站事件采用
//! **官方 Node SDK sidecar**：`node bridge.mjs <app_id> <app_secret>`，
//! 事件以 JSON Lines 从子进程 stdout 推给 Rust。
//!
//! 依赖：用户机器需有 Node.js ≥ 18 与 npm（本应用的 MCP stdio 服务器生态
//! 本就依赖 Node，不引入新门槛）。SDK 首次使用自动 `npm install` 到
//! `app_data/im_inbox/feishu-bridge/`，之后离线可用。

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::models::AppError;

/// 内嵌桥脚本（与本文件同目录 bridge.mjs）
const BRIDGE_SCRIPT: &str = include_str!("bridge.mjs");
const SDK_PACKAGE: &str = "@larksuiteoapi/node-sdk";

/// sidecar 工作目录：`app_data_dir/im_inbox/feishu-bridge/`
pub fn bridge_dir(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    use tauri::Manager;
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::internal(format!("无法解析应用数据目录: {}", e)))?;
    Ok(base.join("im_inbox").join("feishu-bridge"))
}

fn node_bin() -> &'static str {
    "node"
}

fn npm_bin() -> &'static str {
    if cfg!(windows) {
        "npm.cmd"
    } else {
        "npm"
    }
}

/// 确保桥脚本与 SDK 就绪：写脚本 → （首次）npm install。
/// 返回 bridge.mjs 绝对路径。
pub async fn ensure_ready(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    let dir = bridge_dir(app)?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::internal(format!("创建桥目录失败: {}", e)))?;
    let script_path = dir.join("bridge.mjs");
    std::fs::write(&script_path, BRIDGE_SCRIPT)
        .map_err(|e| AppError::internal(format!("写入桥脚本失败: {}", e)))?;
    // package.json 固定 type=module，保证 .mjs import 语义一致
    let pkg = dir.join("package.json");
    if !pkg.exists() {
        std::fs::write(&pkg, r#"{"name":"feishu-bridge","private":true,"type":"module"}"#)
            .map_err(|e| AppError::internal(format!("写入 package.json 失败: {}", e)))?;
    }
    let sdk_marker = dir.join("node_modules").join("@larksuiteoapi");
    if !sdk_marker.exists() {
        install_sdk(&dir).await?;
    }
    Ok(script_path)
}

async fn install_sdk(dir: &std::path::Path) -> Result<(), AppError> {
    let output = Command::new(npm_bin())
        .args(["install", SDK_PACKAGE, "--no-audit", "--no-fund", "--loglevel", "error"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            AppError::configuration(format!(
                "启动 npm 失败（请确认已安装 Node.js/npm）: {}",
                e
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::configuration(format!(
            "安装飞书 SDK 失败: {}",
            stderr.chars().take(300).collect::<String>()
        )));
    }
    Ok(())
}

/// 一条来自 sidecar 的桥事件（stdout JSON Lines）
#[derive(Debug, Clone)]
pub struct BridgeEvent {
    pub name: String,
    pub data: serde_json::Value,
}

/// 启动 sidecar 并返回逐行事件流读取器。
/// 只有一条连接（长连接模式为集群模式，多连接会被随机分摊事件）。
pub async fn spawn(
    app: &tauri::AppHandle,
    app_id: &str,
    app_secret: &str,
) -> Result<BufReader<tokio::process::ChildStdout>, AppError> {
    let script = ensure_ready(app).await?;
    let child = Command::new(node_bin())
        .arg(&script)
        .arg(app_id)
        .arg(app_secret)
        .current_dir(bridge_dir(app)?)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            AppError::configuration(format!(
                "启动飞书桥失败（请确认已安装 Node.js ≥ 18）: {}",
                e
            ))
        })?;
    let stdout = child.stdout.ok_or_else(|| {
        AppError::internal("飞书桥子进程 stdout 不可用")
    })?;
    Ok(BufReader::new(stdout))
}

/// 读取一行事件并解析；返回 None 表示本行无事件（日志/空行/流结束）。
///
/// 官方 SDK 的 logger 会把 `[info]:`/`[error]:` 等非 JSON 行直接打到 stdout，
/// 这些行必须静默跳过而不是当错误断线（实测冒烟验证过该行为）。
pub async fn read_event<R>(reader: &mut BufReader<R>) -> Result<Option<BridgeEvent>, AppError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .map_err(|e| AppError::internal(format!("读取桥事件失败: {}", e)))?;
    if n == 0 {
        return Ok(None);
    }
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    if !line.starts_with('{') {
        // SDK 日志行：含 error 关键字时作为 bridge.error 事件上抛（UI 活动日志可见），
        // 但不触发重连
        if line.to_ascii_lowercase().contains("error") {
            return Ok(Some(BridgeEvent {
                name: "bridge.error".to_string(),
                data: serde_json::Value::String(line.to_string()),
            }));
        }
        return Ok(None);
    }
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!("[feishubot][bridge] 非 JSON 行已忽略: {}", line);
            return Ok(None);
        }
    };
    // stderr 类诊断行：{"type":"log"|"error", "message": "..."} —— 上抛给插件层
    match v.get("type").and_then(|t| t.as_str()) {
        Some("log") | Some("error") => {
            let name = if v.get("type").and_then(|t| t.as_str()) == Some("error") {
                "bridge.error"
            } else {
                "bridge.log"
            };
            Ok(Some(BridgeEvent {
                name: name.to_string(),
                data: v
                    .get("message")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            }))
        }
        _ => Ok(Some(BridgeEvent {
            name: v
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string(),
            data: v.get("data").cloned().unwrap_or(serde_json::Value::Null),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_bin_names_by_platform() {
        // 只验证有值，路径细节由运行环境决定
        assert!(!npm_bin().is_empty());
    }

    #[tokio::test]
    async fn read_event_parses_json_line() {
        let data = b"{\"name\":\"im.message.receive_v1\",\"data\":{\"x\":1}}\n";
        let mut reader = BufReader::new(&data[..]);
        let ev = read_event(&mut reader).await.unwrap().unwrap();
        assert_eq!(ev.name, "im.message.receive_v1");
        assert_eq!(ev.data["x"], 1);
    }

    #[tokio::test]
    async fn read_event_surfaces_log_lines() {
        let data = b"{\"type\":\"log\",\"message\":\"connected\"}\n";
        let mut reader = BufReader::new(&data[..]);
        let ev = read_event(&mut reader).await.unwrap().unwrap();
        assert_eq!(ev.name, "bridge.log");
        assert_eq!(ev.data, "connected");
    }

    #[tokio::test]
    async fn read_event_surfaces_bridge_errors() {
        let data =
            b"{\"type\":\"error\",\"message\":\"ws handshake failed\"}\n[error]: [ 'invalid appId' ]\n";
        let mut reader = BufReader::new(&data[..]);
        let ev = read_event(&mut reader).await.unwrap().unwrap();
        assert_eq!(ev.name, "bridge.error");
        assert_eq!(ev.data, "ws handshake failed");
        let ev = read_event(&mut reader).await.unwrap().unwrap();
        assert_eq!(ev.name, "bridge.error");
        assert!(ev.data.as_str().unwrap().contains("invalid appId"));
    }

    #[tokio::test]
    async fn read_event_tolerates_sdk_logger_lines() {
        // 实测官方 SDK 会往 stdout 打非 JSON 日志：info 行跳过，error 行上抛为 bridge.error
        let data = b"[info]: [ 'event-dispatch is ready' ]\n[error]: [ '[ws]', 'invalid appId: x' ]\n";
        let mut reader = BufReader::new(&data[..]);
        assert!(read_event(&mut reader).await.unwrap().is_none());
        let ev = read_event(&mut reader).await.unwrap().unwrap();
        assert_eq!(ev.name, "bridge.error");
        assert!(ev.data.as_str().unwrap().contains("invalid appId"));
    }
}
