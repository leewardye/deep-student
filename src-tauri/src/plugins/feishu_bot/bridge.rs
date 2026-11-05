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

/// 在常见安装位置探测 node.exe（App 进程的 PATH 往往比终端窄，GUI 启动的
/// 进程拿不到 shell 里注入的 PATH，需要主动兜底探测）。
/// 返回 node.exe 的绝对路径；PATH 可用（`node --version` 成功）时返回 None 表示直接用 "node"。
fn find_node_exe_fallback() -> Option<PathBuf> {
    use std::env;
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut push_env = |vars: &[&str]| {
        for v in vars {
            if let Ok(val) = env::var(v) {
                candidates.push(PathBuf::from(val).join("nodejs").join("node.exe"));
            }
        }
    };
    push_env(&["ProgramFiles", "ProgramFiles(x86)"]);
    if let Ok(appdata) = env::var("APPDATA") {
        let nvm = PathBuf::from(&appdata).join("nvm");
        candidates.push(nvm.join("node.exe"));
        // nvm-windows: %APPDATA%\nvm\v22.x.x\node.exe，取字典序最大（即最新）
        if let Ok(rd) = std::fs::read_dir(&nvm) {
            let mut versions: Vec<PathBuf> = rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.join("node.exe").exists())
                .collect();
            versions.sort();
            if let Some(latest) = versions.pop() {
                candidates.push(latest.join("node.exe"));
            }
        }
    }
    if let Ok(lad) = env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(&lad).join("Volta").join("bin").join("node.exe"));
        candidates.push(
            PathBuf::from(&lad)
                .join("Programs")
                .join("nodejs")
                .join("node.exe"),
        );
    }
    // WorkBuddy 内置 node（装有 WorkBuddy 的机器上必然存在）
    if let Ok(home) = env::var("USERPROFILE") {
        let wb = PathBuf::from(&home)
            .join(".workbuddy")
            .join("binaries")
            .join("node")
            .join("versions");
        if let Ok(rd) = std::fs::read_dir(&wb) {
            let mut versions: Vec<PathBuf> = rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.join("node.exe").exists())
                .collect();
            versions.sort();
            if let Some(latest) = versions.pop() {
                candidates.push(latest.join("node.exe"));
            }
        }
        candidates.push(PathBuf::from(&home).join("scoop").join("apps").join("nodejs").join("current").join("node.exe"));
    }
    candidates.into_iter().find(|p| p.exists())
}

/// 解析可用的 node 可执行文件：优先 PATH（试跑 --version），失败后探测常见目录。
async fn resolve_node_exe() -> Result<(PathBuf, Option<PathBuf>), AppError> {
    // PATH 直接可用？
    let probe = tokio::process::Command::new(node_bin())
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if matches!(probe, Ok(s) if s.success()) {
        return Ok((PathBuf::from(node_bin()), None));
    }
    if let Some(p) = find_node_exe_fallback() {
        return Ok((p.clone(), p.parent().map(|d| d.to_path_buf())));
    }
    Err(AppError::configuration(
        "未找到 Node.js：飞书通道需要 Node.js ≥ 18。请安装 Node.js（https://nodejs.org）后重试，或确认 node 在系统 PATH 中。",
    ))
}

/// node 同目录解析 npm（Windows 完整发行版 npm.cmd 与 node.exe 同目录）。
fn npm_for_node(node_path: &PathBuf, node_dir_hint: Option<PathBuf>) -> String {
    let _ = node_path;
    #[cfg(windows)]
    {
        let npm_name = "npm.cmd";
        if let Some(dir) = &node_dir_hint {
            let p = dir.join(npm_name);
            if p.exists() {
                return p.to_string_lossy().to_string();
            }
        }
        npm_name.to_string()
    }
    #[cfg(not(windows))]
    {
        let _ = (node_path, node_dir_hint);
        "npm".to_string()
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
    let (node_path, node_dir) = resolve_node_exe().await?;
    let npm = npm_for_node(&node_path, node_dir);
    let output = Command::new(&npm)
        .args(["install", SDK_PACKAGE, "--no-audit", "--no-fund", "--loglevel", "error"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            AppError::configuration(format!(
                "启动 npm 失败（npm: {}，请确认已安装 Node.js/npm）: {}",
                npm, e
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

/// 存活的桥子进程。**必须持有 Child 本身**：tokio 的 Child 在 drop 时若
/// kill_on_drop=true 会立即杀掉子进程——曾经 spawn 返回后 Child 被丢弃，
/// node 进程毫秒级被杀，stdout EOF 后主循环忙等，表现为「SDK 就绪但永远
/// 收不到事件且无任何日志」（v0.12.2 修复）。
pub struct BridgeProcess {
    pub child: tokio::process::Child,
    pub stdout: BufReader<tokio::process::ChildStdout>,
    pub stderr: BufReader<tokio::process::ChildStderr>,
}

/// 启动 sidecar。**只有一条连接**（长连接模式为集群模式，多连接会被随机
/// 分摊事件）。stderr 同步返回，由调用方读取——桥的启动错误（node 版本、
/// SDK 加载失败等）都写在 stderr，不读就永远丢失。
pub async fn spawn(
    app: &tauri::AppHandle,
    app_id: &str,
    app_secret: &str,
) -> Result<BridgeProcess, AppError> {
    let script = ensure_ready(app).await?;
    let (node_path, _node_dir) = resolve_node_exe().await?;
    let mut child = Command::new(&node_path)
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
                "启动飞书桥失败（node: {}，请确认已安装 Node.js ≥ 18）: {}",
                node_path.display(),
                e
            ))
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::internal("飞书桥子进程 stdout 不可用"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::internal("飞书桥子进程 stderr 不可用"))?;
    Ok(BridgeProcess {
        child,
        stdout: BufReader::new(stdout),
        stderr: BufReader::new(stderr),
    })
}

/// 读取一行事件并解析。返回 Some(bridge.exit) 表示流结束（子进程退出）；
/// None 表示本行无事件（SDK 日志行/空行）。
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
        // EOF：子进程已退出且 stdout 关闭。必须显式上抛（bridge.exit），
        // 否则调用方会把 None 当空行继续同步读已关闭的流 → 忙旋转死循环
        // （v0.12.2 前的真实 bug：进程被杀后插件看似 Running 却永远无事件）。
        return Ok(Some(BridgeEvent {
            name: "bridge.exit".to_string(),
            data: serde_json::Value::Null,
        }));
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

/// 读取桥进程 stderr 的一行并记录到诊断区（不参与主流程）。
/// 返回读取字节数；EOF 返回 Ok(0)——调用方据此停止 select 该流（否则 EOF
/// 会使分支持续就绪，把 stdout 事件饿死）。桥进程死亡由 stdout 的
/// bridge.exit 事件报告。
pub async fn read_stderr_line<R>(reader: &mut BufReader<R>, buf: &mut String) -> std::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    buf.clear();
    let n = reader.read_line(buf).await?;
    let line = buf.trim_end();
    if !line.is_empty() {
        super::diag_push_line(format!("[stderr] {}", line));
        tracing::warn!("[feishubot][stderr] {}", line);
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_bin_names_by_platform() {
        // npm 名称解析不 panic 即可（路径细节由运行环境决定）
        let n = npm_for_node(&PathBuf::from("node"), None);
        assert!(!n.is_empty());
    }

    #[tokio::test]
    async fn read_event_eof_yields_bridge_exit() {
        let data = b"";
        let mut reader = BufReader::new(&data[..]);
        let ev = read_event(&mut reader).await.unwrap().unwrap();
        assert_eq!(ev.name, "bridge.exit");
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
