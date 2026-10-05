//! 已校验合格候选计划的记录与读取。
//!
//! 固定落盘位置为 `root(data)?/plans/<id>.json`，`UpdatePlan::id` 同时用作 slot UUID。
//! 本模块只包含 DTO 与最小 IO：不新增依赖、不自造运行状态、不自行计算哈希。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::service_install::{root_for, slot_root_for, InstallState, Layout};
use crate::service_updates::PackageMetadata;

/// 计划文件最大字节数。
const MAX_PLAN_BYTES: u64 = 65_536;
/// 计划最大有效时长：24 小时。
const MAX_PLAN_AGE_MILLIS: u64 = 24 * 60 * 60 * 1_000;
/// 列表最多返回的计划条数。
const MAX_LISTED_PLANS: usize = 16;
/// Windows 重解析点属性位。
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// 一个已校验合格候选的更新计划；`id` 兼作 slot UUID。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePlan {
    pub id: String,
    pub service_id: String,
    pub metadata: PackageMetadata,
    pub tree_sha256: String,
    pub base_state: InstallState,
    pub base_manifest_sha256: String,
    pub created_at: u64,
}

/// 当前时间毫秒；时间不可表示时直接报错，不默认、不回退。
fn now_millis() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "系统时间早于 UNIX_EPOCH，无法校验计划时效。".to_string())?
        .as_millis();
    u64::try_from(millis).map_err(|_| "当前时间戳超出 u64 范围。".to_string())
}

/// 当前时间毫秒；把内部时间源的语义原样暴露给更新管理器，不默认、不回退。
pub(crate) fn now() -> Result<u64, String> {
    now_millis()
}

/// Windows 重解析点判定。
#[cfg(windows)]
fn is_reparse(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// 非 Windows 平台无重解析点概念。
#[cfg(not(windows))]
fn is_reparse(_meta: &fs::Metadata) -> bool {
    false
}

/// 计划目录 `<root>/plans`；必须是真实目录，不接受 symlink/重解析点。
fn plans_dir(data: &Path, layout: &Layout, create: bool) -> Result<PathBuf, String> {
    let dir = root_for(data, layout)?.join("plans");
    if fs::symlink_metadata(&dir).is_err() {
        if !create {
            return Err("计划目录不存在。".to_string());
        }
        fs::create_dir_all(&dir).map_err(|_| "计划目录创建失败。".to_string())?;
    }
    let meta = fs::symlink_metadata(&dir).map_err(|_| "计划目录不可访问。".to_string())?;
    if meta.file_type().is_symlink() || is_reparse(&meta) {
        return Err("计划目录不允许是符号链接或重解析点。".to_string());
    }
    if !meta.is_dir() {
        return Err("计划路径不是目录。".to_string());
    }
    Ok(dir)
}

/// 用 `slot_root` 校验 `id` 为 canonical UUID，并返回 `plans/<id>.json`。
fn plan_file(data: &Path, layout: &Layout, id: &str, create: bool) -> Result<PathBuf, String> {
    // `slot_root` 只做 UUID 规范化校验，不要求对应 slot 已存在。
    slot_root_for(data, layout, id)?;
    Ok(plans_dir(data, layout, create)?.join(format!("{id}.json")))
}

/// 64 位小写十六进制校验。
fn is_lowercase_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 计划内容严格校验；任何不合格都报错，不默认、不回退、不打印内容。
///
/// `allow_expired` 只放开 24 小时时效限制（供 active/previous 槽长期读取已封存
/// receipt），其余校验一律不变：“创建时间晚于当前时间”仍被拒绝。
fn validate(
    plan: &UpdatePlan,
    layout: &Layout,
    id: &str,
    allow_expired: bool,
) -> Result<(), String> {
    if plan.id != id {
        return Err("计划 id 与请求不一致。".to_string());
    }
    if plan.service_id != layout.slot {
        return Err("计划 service_id 不合格。".to_string());
    }
    if plan.metadata.name != layout.package {
        return Err("计划包名不合格。".to_string());
    }
    if plan.metadata.version.is_empty() {
        return Err("计划版本为空。".to_string());
    }
    if !is_lowercase_hex64(&plan.tree_sha256) {
        return Err("计划 tree_sha256 不是 64 位小写十六进制。".to_string());
    }
    if !is_lowercase_hex64(&plan.base_manifest_sha256) {
        return Err("计划 base_manifest_sha256 不是 64 位小写十六进制。".to_string());
    }
    let now = now_millis()?;
    if plan.created_at > now {
        return Err("计划创建时间晚于当前时间。".to_string());
    }
    if !allow_expired && now - plan.created_at > MAX_PLAN_AGE_MILLIS {
        return Err("计划已超过 24 小时有效期。".to_string());
    }
    Ok(())
}

/// 独占写入计划；已存在（含旧计划）时直接报错，绝不覆盖。
pub(crate) fn save_plan(data: &Path, layout: &Layout, plan: &UpdatePlan) -> Result<(), String> {
    let path = plan_file(data, layout, &plan.id, true)?;
    let bytes = serde_json::to_vec(plan).map_err(|_| "计划序列化失败。".to_string())?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| "计划已存在或无法独占创建。".to_string())?;
    file.write_all(&bytes)
        .map_err(|_| "计划写入失败。".to_string())?;
    file.sync_all().map_err(|_| "计划落盘失败。".to_string())?;
    Ok(())
}

/// 读取并严格校验计划文件；`allow_expired` 仅决定是否应用 24 小时时效限制。
fn read_plan(
    data: &Path,
    layout: &Layout,
    id: &str,
    allow_expired: bool,
) -> Result<UpdatePlan, String> {
    let path = plan_file(data, layout, id, false)?;
    let meta = fs::symlink_metadata(&path).map_err(|_| "计划文件不存在。".to_string())?;
    if meta.file_type().is_symlink() || is_reparse(&meta) {
        return Err("计划文件不允许是符号链接或重解析点。".to_string());
    }
    if !meta.is_file() {
        return Err("计划路径不是普通文件。".to_string());
    }
    if meta.len() > MAX_PLAN_BYTES {
        return Err("计划文件超过 65536 字节。".to_string());
    }
    let bytes = fs::read(&path).map_err(|_| "计划读取失败。".to_string())?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err("计划文件超过 65536 字节。".to_string());
    }
    let plan: UpdatePlan =
        serde_json::from_slice(&bytes).map_err(|_| "计划解码失败。".to_string())?;
    validate(&plan, layout, id, allow_expired)?;
    Ok(plan)
}

/// 读取并严格校验新 apply 候选计划；任何不合格都报错，不默认、不回退。
pub(crate) fn load_plan(data: &Path, layout: &Layout, id: &str) -> Result<UpdatePlan, String> {
    read_plan(data, layout, id, false)
}

/// 读取 active/previous 槽已封存计划的 receipt：与 `load_plan` 同样严格
/// （路径/UUID/大小/schema/service/包名/哈希，未来时间戳仍拒绝），
/// 但不限制 24 小时年龄，使过期计划仍可被长期读取以支持回退。
pub(crate) fn load_receipt(data: &Path, layout: &Layout, id: &str) -> Result<UpdatePlan, String> {
    read_plan(data, layout, id, true)
}

/// 列出 `root(data)?/plans` 下仍可用的计划，按创建时间倒序，最多 16 条。
///
/// 仅当 `root(data)?/plans` 真实 NotFound（含 root 缺失）时返回空列表；
/// root 本身不可用、目录为 symlink/重解析点、非目录、权限错误或读取目录/目录项
/// 失败都必须报错。单个条目不合格（文件名非 UUID、非 `.json`、非普通文件、
/// 坏计划、过期计划）只跳过该条，不拖垮整次列举。
/// 只读取计划文件，不接触任何会话内容，也不输出原始 JSON。
pub(crate) fn list_plans(data: &Path, layout: &Layout) -> Result<Vec<UpdatePlan>, String> {
    // 只有 `<root>/plans` 真实 NotFound 才表示尚无计划；root 不可用、
    // symlink/重解析点、坏目录、权限错误都必须上报，不能伪装成空列表。
    match fs::symlink_metadata(root_for(data, layout)?.join("plans")) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("计划目录不可访问。".to_string()),
        Ok(_) => {}
    }
    // 目录已确认存在，仍由 `plans_dir` 复核 symlink/重解析点/目录类型。
    let dir = plans_dir(data, layout, false)?;
    let entries = fs::read_dir(&dir).map_err(|_| "计划目录读取失败。".to_string())?;
    let mut plans: Vec<UpdatePlan> = Vec::new();
    for entry in entries {
        // 目录项读取失败是目录级错误，必须上报，不能当作“没有计划”。
        let entry = entry.map_err(|_| "计划目录项读取失败。".to_string())?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        // 只接受普通文件：目录、符号链接、重解析点一律跳过。
        match entry.file_type() {
            Ok(kind) if kind.is_file() => {}
            _ => continue,
        }
        // 文件名 stem 必须能转成 str，随后由 load_plan 校验 canonical UUID。
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        // 坏计划、过期计划、id 与文件名不一致都只跳过这一条。
        if let Ok(plan) = load_plan(data, layout, id) {
            plans.push(plan);
        }
    }
    // 新的在前；时间相同按 id 降序，保证顺序稳定。
    plans.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    plans.truncate(MAX_LISTED_PLANS);
    Ok(plans)
}
