//! 学习者画像（策展长期层）设置页读写命令
//!
//! 读取与 prompt 注入同源（DB 快照）；保存走 [`learner_profile::save_profile_from_form`]
//! 全量权威写回（表单是显式用户操作，允许清空字段），与 AI 工具路径的
//! merge 更新（`learner_profile_update`，防清空）并存、互不冲突（CAS 并发安全）。

use std::sync::Arc;

use serde_json::{json, Value};
use tauri::State;

use crate::commands::AppState;
use crate::memory::learner_profile::{self, LearnerProfile, LEARNER_PROFILE_MAX_CHARS};
use crate::memory::{MemoryOpSource, MemoryService};
use crate::models::AppError;
use crate::vfs::lance_store::VfsLanceStore;

type Result<T> = std::result::Result<T, AppError>;

/// 构造 MemoryService（与 memory_executor::get_service 同序：托管单例优先）
fn build_service(state: &State<'_, AppState>) -> Result<MemoryService> {
    let vfs_db = state
        .vfs_db
        .as_ref()
        .ok_or_else(|| AppError::internal("VFS 数据库不可用"))?
        .clone();
    let lance_store = crate::chat_v2::pipeline::managed_vfs_lance_store_for(&vfs_db)
        .map(Ok)
        .unwrap_or_else(|| VfsLanceStore::new(vfs_db.clone()).map(Arc::new))
        .map_err(|e| AppError::internal(format!("Lance store 初始化失败: {}", e)))?;
    Ok(MemoryService::new(
        vfs_db,
        lance_store,
        state.llm_manager.clone(),
    ))
}

/// 读取学习者画像（未建立时 found=false）
#[tauri::command]
pub async fn learner_profile_get(state: State<'_, AppState>) -> Result<Value> {
    let vfs_db = state
        .vfs_db
        .as_ref()
        .ok_or_else(|| AppError::internal("VFS 数据库不可用"))?
        .clone();
    // 只读路径不需要 lance/LLM，直接走 DB 快照读取（与 prompt 注入同源）
    let load_task = tauri::async_runtime::spawn_blocking(move || {
        learner_profile::load_profile_from_db(&vfs_db)
    });
    let profile = load_task
        .await
        .map_err(|e| AppError::internal(format!("画像读取任务失败: {}", e)))?
        .map_err(|e| AppError::internal(format!("画像读取失败: {}", e)))?;

    Ok(match profile {
        Some(profile) => json!({
            "found": true,
            "version": profile.version,
            "updated_at": profile.updated_at,
            "profile": serde_json::to_value(&profile).unwrap_or(Value::Null),
            "rendered_markdown": profile.render_markdown(),
        }),
        None => json!({
            "found": false,
            "hint": "画像尚未建立：可在下方表单填写，或点击「AI 辅助完善」让对话自动积累。"
        }),
    })
}

/// 表单全量保存（设置页专用，字段可清空）
///
/// 与 AI 的 merge 更新路径并存：这里以表单内容为权威覆盖薄弱点/偏好/目标/
/// 近期状态四块，内部字段（版本/晋升指纹）保留并递增版本。CAS 重试保证与
/// 并发的 AI 写入不互相覆盖丢内容。
#[tauri::command]
pub async fn learner_profile_save(
    profile: LearnerProfile,
    state: State<'_, AppState>,
) -> Result<Value> {
    // 敏感信息过滤（与其他记忆写路径一致）
    let profile_json = serde_json::to_string(&profile).unwrap_or_default();
    if crate::memory::auto_extractor::MemoryAutoExtractor::contains_sensitive_pattern_pub(
        &profile_json,
    ) {
        return Err(AppError::validation(
            "内容包含敏感信息（手机号/身份证/银行卡/邮箱/密码），已拦截。",
        ));
    }

    let service = build_service(&state)?;
    let save_task = tauri::async_runtime::spawn_blocking(move || {
        learner_profile::save_profile_from_form(
            &service,
            &profile,
            MemoryOpSource::Handler,
            "设置页学习者画像表单",
        )
    });
    let outcome = save_task
        .await
        .map_err(|e| AppError::internal(format!("画像保存任务失败: {}", e)))?
        .map_err(|e| {
            // 超过策展上限时给用户可读的错误
            let msg = e.to_string();
            if msg.contains("字符") && msg.contains(&LEARNER_PROFILE_MAX_CHARS.to_string()) {
                AppError::validation(format!(
                    "画像超出 {} 字符上限，请精简后再保存。原始错误：{}",
                    LEARNER_PROFILE_MAX_CHARS, msg
                ))
            } else {
                AppError::internal(format!("画像保存失败: {}", msg))
            }
        })?;

    Ok(json!({
        "success": true,
        "changed": outcome.changed,
        "version": outcome.profile.version,
        "updated_at": outcome.profile.updated_at,
    }))
}
