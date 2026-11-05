//! 远程审批（IM 远程任务的写操作批准，v0.13.0）。
//!
//! 远程任务会话（IM 创建/推进）默认只读；对飞书私聊绑定会话，额外放行
//! [`REMOTE_WRITE_TOOLS`] 白名单内的写工具。这些工具敏感度为 Medium，会走
//! ApprovalManager 审批流——本模块把审批请求**推送到用户的飞书私聊**，
//! 用户回复「批准 <码>」/「拒绝 <码>」后，经 task_command 解析回灌审批结果。
//!
//! 安全边界（fail-closed）：
//! - 写白名单按会话判定：只有「远程任务会话 + 飞书路由」才放行，桌面端
//!   正常会话、自动化会话、微信会话一律维持原 headless 只读白名单；
//! - 写工具仍是 Medium 敏感度，**每次调用都需人工批准**，不提供记忆/预授权；
//! - 审批条目带超时，超时未响应由 ApprovalManager 原有超时逻辑拒绝。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tauri::Manager;

use super::approval_manager::{ApprovalManager, ApprovalResponse};
use super::task_command::CHANNEL_FEISHU;

/// 远程任务会话放行的写工具（canonical 裸名，registry 名带 builtin- 前缀）。
///
/// 仅限「学习资料写入」场景：建题 / 批量导入 / 文档导入 / 提交作答 / 收藏。
/// **不含**任何删除、批量改写、进度重置类工具（Medium 也一律不放），
/// High/Irreversible 工具永远不可能出现在远程会话。
/// 注意：与 `headless::remote_task_write_tool_schemas()` 一一对应——
/// 加白名单必须同时补 schema，否则模型看不见该工具。
pub const REMOTE_TASK_WRITE_TOOLS: &[&str] = &[
    "qbank_create_question",
    "qbank_batch_import",
    "qbank_import_document",
    "qbank_submit_answer",
    "qbank_toggle_favorite",
];

/// 一条待审批的远程写操作
#[derive(Debug, Clone)]
pub struct RemoteApprovalEntry {
    pub session_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub description: String,
    /// 4 位十六进制短码（用户回复「批准 A1B2」用）
    pub code: String,
    pub timeout_seconds: u32,
    pub created_at_ms: i64,
}

type SenderFn = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

static ENTRIES: OnceLock<Mutex<Vec<RemoteApprovalEntry>>> = OnceLock::new();
/// session_id -> (channel, route_id)。由 run_remote_task 在会话确立后登记。
static ROUTES: OnceLock<Mutex<HashMap<String, (String, String)>>> = OnceLock::new();
/// IM 推送闭包（由飞书插件启动时注册；微信通道暂不支持主动推送，不注册）
static SENDER: OnceLock<Mutex<Option<SenderFn>>> = OnceLock::new();

fn entries() -> &'static Mutex<Vec<RemoteApprovalEntry>> {
    ENTRIES.get_or_init(|| Mutex::new(Vec::new()))
}

fn routes() -> &'static Mutex<HashMap<String, (String, String)>> {
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sender() -> &'static Mutex<Option<SenderFn>> {
    SENDER.get_or_init(|| Mutex::new(None))
}

/// 登记远程会话的路由（run_remote_task 在会话创建/复用后调用）。
/// 同一会话推进时覆盖更新；条目量小（会话数级别），不做淘汰。
pub fn remember_route(session_id: &str, channel: &str, route_id: &str) {
    routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.to_string(), (channel.to_string(), route_id.to_string()));
}

/// 查询远程会话的路由：`Some((channel, route_id))`
pub fn route_for(session_id: &str) -> Option<(String, String)> {
    routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .cloned()
}

/// 注册 IM 推送闭包（飞书插件启动时调用一次；后注册者覆盖前者）。
/// 闭包参数：(channel, route_id, text)。推送是异步 fire-and-forget：
/// 闭包内部自行 spawn 异步任务，立即返回（审批不依赖推送结果——
/// 用户也可在桌面端完成审批）。
pub fn register_sender(f: SenderFn) {
    *sender().lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
}

/// 由 tool_call_id 派生稳定的 4 位短码（与 pending 表内现存码去重）
fn make_code(tool_call_id: &str, taken: &[String]) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in tool_call_id.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    let mut n = h & 0xffff;
    loop {
        let code = format!("{:04X}", n);
        if !taken.iter().any(|c| c.eq_ignore_ascii_case(&code)) {
            return code;
        }
        n = (n + 1) & 0xffff;
    }
}

/// 审批请求推送到用户 IM（hooks.rs 在注册审批等待后调用）。
///
/// 仅飞书路由会实际推送；其他通道静默跳过（写白名单也只对飞书放行，
/// 双保险）。推送失败仅记日志——审批仍可由用户在桌面端完成。
pub fn push_approval(
    session_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    description: &str,
    timeout_seconds: u32,
) {
    let Some((channel, route)) = route_for(session_id) else {
        return;
    };
    if channel != CHANNEL_FEISHU {
        return; // 微信通道暂无主动推送，写白名单也不会对其放行
    }
    let push = sender().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(push) = push else {
        return;
    };

    let now = chrono::Utc::now().timestamp_millis();
    let mut guard = entries().lock().unwrap_or_else(|e| e.into_inner());
    // 淘汰过期与超量条目
    guard.retain(|e| now - e.created_at_ms < (e.timeout_seconds as i64 + 120) * 1000);
    while guard.len() >= 32 {
        guard.remove(0);
    }
    let taken: Vec<String> = guard.iter().map(|e| e.code.clone()).collect();
    let code = make_code(tool_call_id, &taken);
    let short_tool = tool_name
        .strip_prefix("builtin-")
        .unwrap_or(tool_name)
        .to_string();
    let text = format!(
        "⚠️ 任务请求执行写操作，需要你确认\n操作：{}\n说明：{}\n\n回复「批准 {}」执行；回复「拒绝 {}」放弃。\n（{} 秒内有效，超时自动取消）",
        short_tool, description, code, code, timeout_seconds
    );
    push(&channel, &route, &text);
    guard.push(RemoteApprovalEntry {
        session_id: session_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        description: description.to_string(),
        code,
        timeout_seconds,
        created_at_ms: now,
    });
}

/// 用户回复「批准/拒绝 [码]」后的响应入口（task_command dispatch 调用）。
///
/// `session_hint` = 当前路由绑定的会话 ID（可为 None——此时必须带码）。
/// 返回应回复给用户的消息文本。
pub fn resolve(
    app: &tauri::AppHandle,
    session_hint: Option<&str>,
    code: &str,
    approve: bool,
) -> String {
    let now = chrono::Utc::now().timestamp_millis();
    let mut guard = entries().lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|e| now - e.created_at_ms < (e.timeout_seconds as i64 + 120) * 1000);

    let code_trim = code.trim();
    let entry = if !code_trim.is_empty() {
        guard
            .iter()
            .find(|e| e.code.eq_ignore_ascii_case(code_trim))
            .cloned()
    } else {
        let in_session: Vec<&RemoteApprovalEntry> = guard
            .iter()
            .filter(|e| session_hint.is_some_and(|s| e.session_id == s))
            .collect();
        if in_session.len() == 1 {
            Some(in_session[0].clone())
        } else {
            let list = guard
                .iter()
                .map(|e| format!("{}（{}）", e.code, e.tool_name))
                .collect::<Vec<_>>()
                .join("、");
            return if list.is_empty() {
                "当前没有待确认的写操作。".to_string()
            } else {
                format!("有多个待确认的写操作，请带上短码回复，例如「批准 {}」。待确认：{}", 
                    guard.last().map(|e| e.code.as_str()).unwrap_or(""), list)
            };
        }
    };

    let Some(entry) = entry else {
        return format!(
            "没有找到短码 {} 对应的待确认操作（可能已超时或已处理）。发送「进展」查看任务状态。",
            code_trim
        );
    };

    // 从 pending 表移除（无论后续 delivered 与否，避免重复响应）
    guard.retain(|e| e.tool_call_id != entry.tool_call_id);
    drop(guard);

    let manager = app
        .try_state::<Arc<ApprovalManager>>()
        .map(|m| m.inner().clone());
    let Some(manager) = manager else {
        return "审批通道不可用（ApprovalManager 未初始化），请到桌面端确认。".to_string();
    };
    let response = if approve {
        ApprovalResponse::approved(
            entry.session_id.clone(),
            entry.tool_call_id.clone(),
            entry.tool_name.clone(),
        )
    } else {
        ApprovalResponse::rejected(
            entry.session_id.clone(),
            entry.tool_call_id.clone(),
            entry.tool_name.clone(),
            Some("用户在 IM 拒绝".to_string()),
        )
    };
    let result = manager.respond_with_result(response);
    let verb = if approve { "已批准" } else { "已拒绝" };
    if result.delivered {
        format!("{}：{}（{}）", verb, entry.tool_name, entry.description)
    } else {
        format!(
            "{} 的审批已失效（操作可能已超时或被取消），本次{}未生效。",
            entry.code, verb
        )
    }
}

/// hooks.rs 在审批等待结束（批准/拒绝/超时/取消）后清理对应条目
pub fn remove(tool_call_id: &str) {
    entries()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|e| e.tool_call_id != tool_call_id);
}

/// 远程写工具是否对当前会话放行（headless.rs 工具面扩展用）
pub fn write_tools_allowed_for(session_id: &str) -> bool {
    match route_for(session_id) {
        Some((channel, _)) => channel == CHANNEL_FEISHU,
        None => false,
    }
}
