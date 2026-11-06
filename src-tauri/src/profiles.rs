//! 多用户账套（Profile）管理。
//!
//! 设计（2026-11-05）：
//! - **账套 = 独立数据根**。每个账套拥有自己的数据目录，A/B 双数据空间、
//!   主库/笔记/Anki 等所有数据库、VFS 文件、`.secure` 凭证（模型 Key、
//!   飞书/微信通道凭证）全部落在该目录内，天然互不干扰。
//! - 注册表 `profiles.json` 存放在**应用数据根**（所有账套之外），
//!   记录账套元信息与当前激活账套。
//! - `default`（默认账套）直接使用应用数据根本身，**存量用户数据零迁移**。
//! - 其余账套位于 `<app_data>/profiles/<id>/`。
//! - 切换账套 = 更新激活标记 + 重启应用（`AppHandle::restart()`，与
//!   数据治理清空/恢复切换同一模式，不做热切换）。
//! - 删除账套 = 目录重命名到 `<app_data>/_deleted_profiles/<id>-<时间戳>`，
//!   不直接 rm，可手工找回。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::models::AppError;

/// 默认账套 ID：数据根 = 应用数据根本身（兼容存量用户，零迁移）
pub const DEFAULT_PROFILE_ID: &str = "default";
/// 注册表文件名（存放在应用数据根，所有账套之外）
const REGISTRY_FILE: &str = "profiles.json";
/// 非默认账套数据目录名
const PROFILES_DIR: &str = "profiles";
/// 删除账套的回收目录
const DELETED_DIR: &str = "_deleted_profiles";
/// 账套名上限（字符数，Unicode 感知）
const NAME_MAX_CHARS: usize = 40;

/// 注册表写入互斥（防止并发命令交错写坏 JSON）
fn registry_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileMeta {
    pub id: String,
    pub name: String,
    pub created_at_ms: u64,
    pub last_used_at_ms: u64,
    // ── 学生身份层（2026-11-06，全部可选，旧注册表零迁移）──
    /// 学生姓名（展示用，区别于账套名 name）
    #[serde(default)]
    pub student_name: Option<String>,
    /// 年级（枚举见 GRADES）
    #[serde(default)]
    pub grade: Option<String>,
    /// 学校（可选）
    #[serde(default)]
    pub school: Option<String>,
    /// 头像 emoji（可选；None 时前端按 id 派生色板 + 首字）
    #[serde(default)]
    pub avatar: Option<String>,
}

/// 合法年级枚举（与作文批改 grade_level 对齐的映射在联动层做）
pub const GRADES: &[&str] = &[
    "primary_1", "primary_2", "primary_3", "primary_4", "primary_5", "primary_6",
    "junior_1", "junior_2", "junior_3",
    "senior_1", "senior_2", "senior_3",
    "college", "other",
];

/// 学生姓名上限（字符数）
const STUDENT_NAME_MAX_CHARS: usize = 20;
/// 学校名上限（字符数）
const SCHOOL_MAX_CHARS: usize = 40;
/// 头像 emoji 上限（字符数，容纳组合 emoji）
const AVATAR_MAX_CHARS: usize = 8;

/// 归一化可选文本字段：trim 后为空 → None；超长报错
fn normalize_optional_text(
    value: Option<String>,
    field_label: &str,
    max_chars: usize,
) -> Result<Option<String>, AppError> {
    match value {
        None => Ok(None),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            if trimmed.chars().count() > max_chars {
                return Err(AppError::validation(format!(
                    "{}不能超过 {} 个字符",
                    field_label, max_chars
                )));
            }
            Ok(Some(trimmed.to_string()))
        }
    }
}

fn validate_grade(grade: Option<String>) -> Result<Option<String>, AppError> {
    match grade {
        None => Ok(None),
        Some(g) => {
            if g.is_empty() {
                return Ok(None);
            }
            if GRADES.contains(&g.as_str()) {
                Ok(Some(g))
            } else {
                Err(AppError::validation(format!("不支持的年级: {}", g)))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileRegistry {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    profiles: Vec<ProfileMeta>,
    /// None = 未初始化（仅出现在手动改坏的文件），按 default 处理
    active_profile_id: Option<String>,
}

fn default_schema_version() -> u32 {
    1
}

impl ProfileRegistry {
    fn default_registry() -> Self {
        let t = now_ms();
        Self {
            schema_version: 1,
            profiles: vec![ProfileMeta {
                id: DEFAULT_PROFILE_ID.to_string(),
                name: "默认账套".to_string(),
                created_at_ms: t,
                last_used_at_ms: t,
                student_name: None,
                grade: None,
                school: None,
                avatar: None,
            }],
            active_profile_id: Some(DEFAULT_PROFILE_ID.to_string()),
        }
    }

    fn find(&self, id: &str) -> Option<&ProfileMeta> {
        self.profiles.iter().find(|p| p.id == id)
    }

    fn active_id(&self) -> &str {
        self.active_profile_id.as_deref().unwrap_or(DEFAULT_PROFILE_ID)
    }
}

fn registry_path(base_app_data_dir: &Path) -> PathBuf {
    base_app_data_dir.join(REGISTRY_FILE)
}

/// 活跃账套登记的年级枚举（供作文批改 grade_level 缺省联动等场景）。
///
/// 只读、无锁争用风险之外无副作用；注册表缺失/解析失败一律返回 None
/// （身份信息缺失不应阻断功能）。
pub fn active_profile_grade() -> Option<String> {
    let app = crate::get_global_app_handle()?;
    let base = app.path().app_data_dir().ok()?;
    let reg = load_registry(&base);
    let meta = reg
        .profiles
        .iter()
        .find(|p| p.id == reg.active_id())?;
    let grade = meta.grade.as_deref()?.trim();
    if grade.is_empty() || grade == "other" {
        return None;
    }
    Some(grade.to_string())
}

fn load_registry(base_app_data_dir: &Path) -> ProfileRegistry {
    let path = registry_path(base_app_data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<ProfileRegistry>(&bytes) {
            Ok(reg) if !reg.profiles.is_empty() => reg,
            Ok(_) => {
                tracing::warn!("[profiles] 注册表为空，重建默认账套: {}", path.display());
                ProfileRegistry::default_registry()
            }
            Err(e) => {
                tracing::warn!("[profiles] 注册表解析失败({})，重建默认账套: {}", e, path.display());
                ProfileRegistry::default_registry()
            }
        },
        Err(_) => ProfileRegistry::default_registry(),
    }
}

fn save_registry(base_app_data_dir: &Path, reg: &ProfileRegistry) -> Result<(), AppError> {
    let path = registry_path(base_app_data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::internal(format!("创建账套注册表目录失败: {}", e)))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(reg)
        .map_err(|e| AppError::internal(format!("序列化账套注册表失败: {}", e)))?;
    // 原子写入：先写临时文件再 rename，避免中途崩溃留半截 JSON
    std::fs::write(&tmp, &bytes)
        .map_err(|e| AppError::internal(format!("写账套注册表失败: {}", e)))?;
    #[cfg(windows)]
    {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
    std::fs::rename(&tmp, &path)
        .map_err(|e| AppError::internal(format!("提交账套注册表失败: {}", e)))?;
    Ok(())
}

/// 指定账套的数据根：default → 应用数据根本身；其余 → profiles/<id>/
pub fn data_root_for(base_app_data_dir: &Path, profile_id: &str) -> PathBuf {
    if profile_id == DEFAULT_PROFILE_ID {
        base_app_data_dir.to_path_buf()
    } else {
        base_app_data_dir.join(PROFILES_DIR).join(profile_id)
    }
}

/// 启动期入口：加载（或首次创建）注册表，返回**当前账套的数据根**。
/// 同时把当前账套的 last_used 刷新后落盘（失败仅告警，不阻断启动）。
/// 注册表损坏时按默认账套重建（fail-safe，不阻断启动）。
pub fn load_or_init(base_app_data_dir: &Path) -> PathBuf {
    let mut reg = load_registry(base_app_data_dir);
    let active_id = reg.active_id().to_string();
    if reg.find(&active_id).is_none() {
        tracing::warn!(
            "[profiles] 激活账套 {} 不存在，回退默认账套",
            active_id
        );
        reg.active_profile_id = Some(DEFAULT_PROFILE_ID.to_string());
    }
    let active = reg.active_id().to_string();
    let root = data_root_for(base_app_data_dir, &active);
    if let Some(meta) = reg.profiles.iter_mut().find(|p| p.id == active) {
        meta.last_used_at_ms = now_ms();
    }
    if let Err(e) = save_registry(base_app_data_dir, &reg) {
        tracing::warn!("[profiles] 刷新 last_used 失败（不影响启动）: {}", e);
    }
    tracing::info!("[profiles] 账套已加载: active={}, data_root={}", active, root.display());
    root
}

/// 只读获取当前激活账套的数据根（不写注册表；供运行期命令定位当前账套目录）
pub fn active_data_root(base_app_data_dir: &Path) -> PathBuf {
    let reg = load_registry(base_app_data_dir);
    data_root_for(base_app_data_dir, reg.active_id())
}

fn validate_name(name: &str) -> Result<String, AppError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(AppError::validation("账套名称不能为空"));
    }
    if trimmed.chars().count() > NAME_MAX_CHARS {
        return Err(AppError::validation(format!(
            "账套名称不能超过 {} 个字符",
            NAME_MAX_CHARS
        )));
    }
    Ok(trimmed.to_string())
}

fn new_profile_id() -> String {
    format!("p-{}", uuid::Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------------
// Tauri 命令
// ---------------------------------------------------------------------------

/// 前端展示结构
#[derive(Debug, Clone, Serialize)]
pub struct ProfileInfo {
    pub id: String,
    pub name: String,
    pub created_at_ms: u64,
    pub last_used_at_ms: u64,
    pub active: bool,
    pub is_default: bool,
    /// 数据根绝对路径（调试/展示用）
    pub data_root: String,
    // ── 学生身份层 ──
    pub student_name: Option<String>,
    pub grade: Option<String>,
    pub school: Option<String>,
    pub avatar: Option<String>,
}

fn to_info(base: &Path, reg: &ProfileRegistry, meta: &ProfileMeta) -> ProfileInfo {
    ProfileInfo {
        id: meta.id.clone(),
        name: meta.name.clone(),
        created_at_ms: meta.created_at_ms,
        last_used_at_ms: meta.last_used_at_ms,
        active: meta.id == reg.active_id(),
        is_default: meta.id == DEFAULT_PROFILE_ID,
        data_root: data_root_for(base, &meta.id).display().to_string(),
        student_name: meta.student_name.clone(),
        grade: meta.grade.clone(),
        school: meta.school.clone(),
        avatar: meta.avatar.clone(),
    }
}

#[tauri::command]
pub fn profile_list(app: tauri::AppHandle) -> Result<Vec<ProfileInfo>, AppError> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::internal(format!("获取应用数据目录失败: {}", e)))?;
    let reg = load_registry(&base);
    Ok(reg
        .profiles
        .iter()
        .map(|m| to_info(&base, &reg, m))
        .collect())
}

#[tauri::command]
pub fn profile_create(
    app: tauri::AppHandle,
    name: String,
) -> Result<ProfileInfo, AppError> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::internal(format!("获取应用数据目录失败: {}", e)))?;
    let name = validate_name(&name)?;
    let _g = registry_lock()
        .lock()
        .map_err(|_| AppError::internal("账套注册表锁不可用"))?;
    let mut reg = load_registry(&base);
    let id = new_profile_id();
    let t = now_ms();
    let meta = ProfileMeta {
        id: id.clone(),
        name,
        created_at_ms: t,
        last_used_at_ms: t,
        student_name: None,
        grade: None,
        school: None,
        avatar: None,
    };
    // 预建数据目录（A/B slots 由 data_space 启动逻辑按需创建，这里只建根）
    let root = data_root_for(&base, &id);
    std::fs::create_dir_all(&root)
        .map_err(|e| AppError::internal(format!("创建账套数据目录失败: {}", e)))?;
    reg.profiles.push(meta.clone());
    save_registry(&base, &reg)?;
    tracing::info!("[profiles] 创建账套 {} ({})", meta.name, id);
    Ok(to_info(&base, &reg, &meta))
}

#[tauri::command]
pub fn profile_rename(
    app: tauri::AppHandle,
    profile_id: String,
    name: String,
) -> Result<(), AppError> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::internal(format!("获取应用数据目录失败: {}", e)))?;
    let name = validate_name(&name)?;
    let _g = registry_lock()
        .lock()
        .map_err(|_| AppError::internal("账套注册表锁不可用"))?;
    let mut reg = load_registry(&base);
    let meta = reg
        .profiles
        .iter_mut()
        .find(|p| p.id == profile_id)
        .ok_or_else(|| AppError::validation(format!("账套 {} 不存在", profile_id)))?;
    meta.name = name;
    save_registry(&base, &reg)
}

/// 更新学生身份字段（全量语义：四个字段都以 Some 传入，空串 = 清除）。
#[tauri::command]
pub fn profile_update(
    app: tauri::AppHandle,
    profile_id: String,
    student_name: Option<String>,
    grade: Option<String>,
    school: Option<String>,
    avatar: Option<String>,
) -> Result<ProfileInfo, AppError> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::internal(format!("获取应用数据目录失败: {}", e)))?;
    let student_name = normalize_optional_text(
        student_name,
        "学生姓名",
        STUDENT_NAME_MAX_CHARS,
    )?;
    let grade = validate_grade(grade)?;
    let school =
        normalize_optional_text(school, "学校名称", SCHOOL_MAX_CHARS)?;
    let avatar = normalize_optional_text(avatar, "头像", AVATAR_MAX_CHARS)?;
    let _g = registry_lock()
        .lock()
        .map_err(|_| AppError::internal("账套注册表锁不可用"))?;
    let mut reg = load_registry(&base);
    let meta = reg
        .profiles
        .iter_mut()
        .find(|p| p.id == profile_id)
        .ok_or_else(|| AppError::validation(format!("账套 {} 不存在", profile_id)))?;
    meta.student_name = student_name;
    meta.grade = grade;
    meta.school = school;
    meta.avatar = avatar;
    save_registry(&base, &reg)?;
    let updated = meta.clone();
    Ok(to_info(&base, &reg, &updated))
}

#[tauri::command]
pub fn profile_delete(app: tauri::AppHandle, profile_id: String) -> Result<(), AppError> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::internal(format!("获取应用数据目录失败: {}", e)))?;
    if profile_id == DEFAULT_PROFILE_ID {
        return Err(AppError::validation("默认账套不能删除"));
    }
    let _g = registry_lock()
        .lock()
        .map_err(|_| AppError::internal("账套注册表锁不可用"))?;
    let mut reg = load_registry(&base);
    if reg.active_id() == profile_id {
        return Err(AppError::validation(
            "不能删除当前使用的账套，请先切换到其他账套",
        ));
    }
    let before = reg.profiles.len();
    reg.profiles.retain(|p| p.id != profile_id);
    if reg.profiles.len() == before {
        return Err(AppError::validation(format!("账套 {} 不存在", profile_id)));
    }
    // 数据目录改名进回收区，不直接删除（可手工找回）
    let root = data_root_for(&base, &profile_id);
    if root.exists() {
        let trash = base.join(DELETED_DIR);
        std::fs::create_dir_all(&trash)
            .map_err(|e| AppError::internal(format!("创建回收目录失败: {}", e)))?;
        let target = trash.join(format!("{}-{}", profile_id, now_ms()));
        std::fs::rename(&root, &target)
            .map_err(|e| AppError::internal(format!("移除账套数据目录失败: {}", e)))?;
        tracing::info!("[profiles] 账套 {} 数据已移至 {}", profile_id, target.display());
    }
    save_registry(&base, &reg)
}

/// 切换账套：写激活标记后重启应用（与数据治理恢复切换同一模式）。
/// `restart()` 不返回；若重启失败才会走到 unreachable 之后的错误。
#[tauri::command]
pub fn profile_switch(app: tauri::AppHandle, profile_id: String) -> Result<(), AppError> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::internal(format!("获取应用数据目录失败: {}", e)))?;
    let _g = registry_lock()
        .lock()
        .map_err(|_| AppError::internal("账套注册表锁不可用"))?;
    let mut reg = load_registry(&base);
    // 在可变借用结束前取出日志所需字段，避免 meta 的 &mut 借用跨越 save_registry 的不可变借用（E0502）
    let (profile_name, profile_id) = {
        let meta = reg
            .profiles
            .iter_mut()
            .find(|p| p.id == profile_id)
            .ok_or_else(|| AppError::validation(format!("账套 {} 不存在", profile_id)))?;
        meta.last_used_at_ms = now_ms();
        (meta.name.clone(), meta.id.clone())
    };
    reg.active_profile_id = Some(profile_id.clone());
    save_registry(&base, &reg)?;
    tracing::info!("[profiles] 切换到账套 {} ({}), 即将重启", profile_name, profile_id);
    app.restart();
    unreachable!("profile_switch: 应用应已重启")
}

// ---------------------------------------------------------------------------
// 单测（纯逻辑部分）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dstu_profiles_test_{}_{}_{}",
            tag,
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_registry_points_at_base() {
        let base = temp_base("default");
        let root = load_or_init(&base);
        let reg = load_registry(&base);
        assert_eq!(reg.active_id(), DEFAULT_PROFILE_ID);
        assert_eq!(reg.profiles.len(), 1);
        assert_eq!(root, base);
        // 注册表已落盘
        assert!(registry_path(&base).exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn non_default_profile_data_root_is_isolated() {
        let base = temp_base("iso");
        let root = data_root_for(&base, "p-abc123");
        assert_ne!(root, base);
        assert!(root.starts_with(&base.join(PROFILES_DIR)));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn registry_roundtrip_and_corrupt_recovery() {
        let base = temp_base("roundtrip");
        let mut reg = load_registry(&base);
        reg.profiles.push(ProfileMeta {
            id: "p-x".into(),
            name: "测试".into(),
            created_at_ms: 1,
            last_used_at_ms: 2,
            student_name: Some("小明".into()),
            grade: Some("junior_2".into()),
            school: None,
            avatar: None,
        });
        save_registry(&base, &reg).unwrap();
        let reloaded = load_registry(&base);
        assert_eq!(reloaded.profiles.len(), 2);
        assert_eq!(reloaded.profiles[1].name, "测试");
        // 学生身份字段 round-trip
        assert_eq!(reloaded.profiles[1].student_name.as_deref(), Some("小明"));
        assert_eq!(reloaded.profiles[1].grade.as_deref(), Some("junior_2"));

        // 旧 schema（无学生字段）也能读，缺省 None
        let legacy = r#"{"schema_version":1,"profiles":[{"id":"p-old","name":"旧","created_at_ms":1,"last_used_at_ms":2}],"active_profile_id":"p-old"}"#;
        std::fs::write(registry_path(&base), legacy).unwrap();
        let old = load_registry(&base);
        assert!(old.profiles[0].student_name.is_none());
        assert!(old.profiles[0].grade.is_none());

        // 写坏 JSON → 重建默认账套（fail-safe，不崩启动）
        std::fs::write(registry_path(&base), b"{not json").unwrap();
        let recovered = load_registry(&base);
        assert_eq!(recovered.active_id(), DEFAULT_PROFILE_ID);
        assert_eq!(recovered.profiles.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn validate_name_rules() {
        assert!(validate_name("  孩子的账套 ").is_ok());
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"长".repeat(NAME_MAX_CHARS + 1)).is_err());
        assert!(validate_name(&"短".repeat(NAME_MAX_CHARS)).is_ok());
    }

    #[test]
    fn normalize_optional_text_rules() {
        assert_eq!(normalize_optional_text(Some("  张三 ".into()), "学生姓名", 20).unwrap(), Some("张三".into()));
        assert_eq!(normalize_optional_text(Some("   ".into()), "学生姓名", 20).unwrap(), None);
        assert_eq!(normalize_optional_text(None, "学生姓名", 20).unwrap(), None);
        assert!(normalize_optional_text(Some("长".repeat(21)), "学生姓名", 20).is_err());
    }

    #[test]
    fn validate_grade_rules() {
        assert_eq!(validate_grade(Some("junior_2".into())).unwrap(), Some("junior_2".into()));
        assert_eq!(validate_grade(Some(String::new())).unwrap(), None);
        assert_eq!(validate_grade(None).unwrap(), None);
        assert!(validate_grade(Some("grade_99".into())).is_err());
    }
}
