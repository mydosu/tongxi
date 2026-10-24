//! 项目：登记、绑定到群、主脑设置和长期授权。协作本身由主脑会话做（见 project_brain）。
use crate::project_store::{Project, RoleChoice};
use crate::store::ConversationDetail;
use crate::AppState;
use std::path::Path;
use tauri::State;

type Result<T> = std::result::Result<T, String>;

fn locked<'a>(state: &'a State<'_, AppState>) -> Result<std::sync::MutexGuard<'a, crate::store::Store>> {
    state.store.lock().map_err(|_| "本地数据暂不可用".into())
}

#[tauri::command]
pub fn list_projects(state: State<'_, AppState>) -> Result<Vec<Project>> {
    locked(&state)?.projects()
}
#[tauri::command]
pub fn register_project(state: State<'_, AppState>, name: String, root: String) -> Result<Project> {
    locked(&state)?.register_project(&name, &root)
}
#[tauri::command]
pub fn bind_project(
    state: State<'_, AppState>,
    conversation_id: String,
    project_id: Option<String>,
) -> Result<()> {
    if let Some(project_id) = project_id.as_deref() {
        let root = {
            let store = locked(&state)?;
            if store.conversation(&conversation_id)?.kind != "group" {
                return Err("只有群聊才能绑定项目".into());
            }
            store.guard_discussion(&conversation_id)?;
            store.project(project_id)?.root
        };
        crate::project_worktree::initialize_safe_repository(Path::new(&root))?;
    }
    let mut store = locked(&state)?;
    store.bind_project(&conversation_id, project_id.as_deref())?;
    store.ensure_project_agent_chats(&conversation_id).map(|_| ())
}
#[tauri::command]
pub fn release_project(state: State<'_, AppState>, project_id: String) -> Result<Vec<Project>> {
    let mut store = locked(&state)?;
    store.release_project(&project_id)?;
    store.projects()
}
#[tauri::command]
pub fn set_project_summary(state: State<'_, AppState>, project_id: String, enabled: bool) -> Result<Project> {
    locked(&state)?.set_project_summary(&project_id, enabled)
}

/// 主脑由哪位成员当、用什么模型和思考强度。
#[tauri::command]
pub fn set_project_coordinator(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    conversation_id: String,
    coordinator: RoleChoice,
) -> Result<ConversationDetail> {
    if let Ok(snapshot) = crate::native_snapshot(&app, &coordinator.agent) {
        if snapshot.connection == "connected" {
            crate::models::validate_selection(
                &snapshot.models,
                coordinator.model.as_deref(),
                coordinator.effort.as_deref(),
                snapshot.default_model.as_deref(),
            )?;
        }
    }
    let mut store = locked(&state)?;
    store.set_project_coordinator(&conversation_id, &coordinator)?;
    // 换了主脑成员就建好它的主脑会话，模型和思考强度同步过去。
    store.ensure_project_agent_chats(&conversation_id)?;
    store.detail(&conversation_id)
}

/// 项目权限档：read 只读、write 只改工作树、full 全权；成员下一回合起按新档位跑。
#[tauri::command]
pub fn set_project_permission(state: State<'_, AppState>, conversation_id: String, tier: String) -> Result<ConversationDetail> {
    let mut store = locked(&state)?;
    store.set_permission_tier(&conversation_id, &tier)?;
    store.detail(&conversation_id)
}

/// 项目里各成员每天的花费上限（成员 → 美元），没列的成员不限。
#[tauri::command]
pub fn set_project_spend_cap(state: State<'_, AppState>, conversation_id: String, caps: std::collections::BTreeMap<String, f64>) -> Result<ConversationDetail> {
    let mut store = locked(&state)?;
    store.set_spend_cap(&conversation_id, caps)?;
    store.detail(&conversation_id)
}

/// 项目长期授权：用户写一段话（如「允许提交并推送 main，不 force push」），主脑和成员都当作用户原话授权。
#[tauri::command]
pub fn set_project_grants(
    state: State<'_, AppState>,
    conversation_id: String,
    grants: String,
) -> Result<ConversationDetail> {
    let mut store = locked(&state)?;
    store.set_project_grants(&conversation_id, &grants)?;
    store.detail(&conversation_id)
}
