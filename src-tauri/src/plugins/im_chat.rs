//! IM 闲聊意图分类 + 闲聊 system prompt（微信/飞书共用）。
//!
//! 需求（2026-11-03 与用户确认）：
//! - 简单对话不走任务系统观感：AI 自动判断「闲聊 vs 要做的事」
//! - 闲聊复用既有任务机器（会话历史/去重/限流全继承），仅切换提示词并静默启动
//! - 分类器走 model2 工具模型（快、便宜），失败时保守回退为任务（保持旧行为）

use tauri::Manager;

use crate::models::AppError;

/// 闲聊轮的 system prompt 追加段。
/// 注意与任务模式（DEFAULT_REMOTE_SYSTEM_APPEND）的差别：对话式、不带任务汇报腔。
pub const CHAT_SYSTEM_APPEND: &str = "你正在通过 IM（微信/飞书）和用户闲聊。这不是一个任务，就是普通对话：\n- 回复自然、口语化，像朋友之间发消息，不要用「任务已完成」这类汇报腔\n- 简体中文，长度随内容走：简单问题一两句话就够，别硬凑结构\n- 可以解答学习问题、聊聊想法；用户如果想让你做一件具体的事（整理、生成、批改、出题等），直接做即可，做完用一句话说明结果\n- 不要主动列命令清单、不要提「桌面端」除非用户问到";

/// 意图分类提示词模板
fn classify_prompt(recent: &[String], text: &str) -> String {
    let history = if recent.is_empty() {
        String::from("（无）")
    } else {
        recent
            .iter()
            .map(|s| format!("- {}", s.chars().take(60).collect::<String>()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "判断用户这条 IM 消息是「闲聊」还是「要 agent 做一件具体的事」。\n\
         最近消息（供参考）：\n{}\n\
         最新消息：{}\n\
         判定标准：问候、寒暄、提问请教、感想、追问等对话性质 → chat；\
         整理/生成/批改/出题/总结/翻译/写笔记/查资料等需要动手完成的 → task；\
         拿不准时倾向 task。\n\
         只回复一个词：chat 或 task",
        history,
        text.chars().take(200).collect::<String>()
    )
}

/// 判断一条消息是否闲聊意图。
///
/// - LLM 不可用 / 输出无法解析 → false（保守回退：按任务处理，不改变旧行为）
/// - 仅对解析为普通消息的文本调用（命令/帮助不走这里，由调用方保证）
pub async fn is_chat_intent(
    app: &tauri::AppHandle,
    recent: &[String],
    text: &str,
) -> Result<bool, AppError> {
    let manager = app
        .try_state::<std::sync::Arc<crate::llm_manager::LLMManager>>()
        .ok_or_else(|| AppError::internal("LLMManager 未初始化"))?
        .inner()
        .clone();
    let prompt = classify_prompt(recent, text);
    let output = manager
        .call_model2_raw_prompt(
            &prompt,
            None,
            crate::llm_usage::CallerType::Other("im_intent".into()),
        )
        .await
        .map_err(|e| AppError::internal(format!("意图分类调用失败: {}", e)))?;
    let answer = output.assistant_message.trim().to_ascii_lowercase();
    Ok(answer.starts_with("chat") || (answer.contains("chat") && !answer.contains("task")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_prompt_contains_rules_and_text() {
        let p = classify_prompt(&["在吗".into()], "帮我出 5 道三角函数题");
        assert!(p.contains("chat 或 task"));
        assert!(p.contains("在吗"));
        assert!(p.contains("帮我出 5 道三角函数题"));
    }

    #[test]
    fn classify_prompt_truncates_long_text() {
        let long = "测".repeat(500);
        let p = classify_prompt(&[], &long);
        assert!(p.chars().count() < 1000, "超长文本必须截断");
    }
}
