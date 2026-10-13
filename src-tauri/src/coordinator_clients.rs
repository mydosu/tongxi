use crate::{
    claude, codex, coordinator_policy::MANAGER_ACTIVITY, dsh, hermes, project_store::Attempt,
    rpc::Client, store::Store,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

struct Entry {
    client: Arc<Client>,
    loaded: Option<String>,
}
pub struct Pool {
    store: Arc<Mutex<Store>>,
    codex: Arc<codex::Runtime>,
    hermes: Arc<hermes::Runtime>,
    dsh: Arc<dsh::Runtime>,
    claude: Arc<claude::Runtime>,
    entries: Mutex<HashMap<String, Entry>>,
    root: PathBuf,
}
pub fn codex_arguments() -> Vec<&'static str> {
    vec![
        "-c",
        "mcp_servers={}",
        "-c",
        "approval_reviewer=\"user\"",
        "-c",
        "plugins={}",
        "-c",
        "features.shell_tool=false",
        "-c",
        "features.unified_exec=false",
        "-c",
        "features.apply_patch_freeform=false",
        "-c",
        "features.collab=false",
        "-c",
        "features.multi_agent=false",
        "-c",
        "features.hooks=false",
        "-c",
        "features.remote_plugin=false",
        "-c",
        "features.memories=false",
        "-c",
        "features.skills=false",
        "-c",
        "features.js_repl=false",
        "-c",
        "tools.view_image=false",
        "-c",
        "web_search=\"disabled\"",
        "app-server",
        "--listen",
        "stdio://",
    ]
}
impl Pool {
    pub fn new(
        store: Arc<Mutex<Store>>,
        codex: Arc<codex::Runtime>,
        hermes: Arc<hermes::Runtime>,
        dsh: Arc<dsh::Runtime>,
        claude: Arc<claude::Runtime>,
    ) -> Arc<Self> {
        let root = store
            .lock()
            .unwrap()
            .path
            .parent()
            .unwrap()
            .join("coordinators");
        Arc::new(Self {
            store,
            codex,
            hermes,
            dsh,
            claude,
            root,
            entries: Mutex::new(HashMap::new()),
        })
    }
    pub fn directory(&self, id: &str, agent: &str) -> PathBuf {
        self.root.join(id).join(agent)
    }
    pub fn client(&self, attempt: &Attempt) -> Result<Arc<Client>, String> {
        if attempt.activity.as_deref() != Some(MANAGER_ACTIVITY) {
            return Err("不是管家回合".into());
        }
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get(&attempt.workflow_id) {
            return Ok(entry.client.clone());
        }
        let manager = self
            .store
            .lock()
            .unwrap()
            .coordination(&attempt.workflow_id)?
            .ok_or("管家配置缺失")?;
        if manager.coordinator.agent != attempt.agent_id {
            return Err("管家身份不匹配".into());
        }
        let directory = self.directory(&attempt.workflow_id, &attempt.agent_id);
        let client = match attempt.agent_id.as_str() {
            "codex-win" => self.codex.coordinator_client(&directory),
            "hermes-win" => self.hermes.coordinator_client(&directory),
            "dsh-win" => self.dsh.coordinator_client(&directory),
            "claude-win" => self.claude.coordinator_client(&directory),
            _ => Err("管家成员无效".into()),
        }?;
        entries.insert(
            attempt.workflow_id.clone(),
            Entry {
                client: client.clone(),
                loaded: None,
            },
        );
        Ok(client)
    }
    pub fn loaded(&self, attempt: &Attempt, session: &str) -> bool {
        self.entries
            .lock()
            .unwrap()
            .get(&attempt.workflow_id)
            .is_some_and(|entry| entry.loaded.as_deref() == Some(session))
    }
    pub fn ready(&self, attempt: &Attempt, session: &str) {
        if let Some(entry) = self.entries.lock().unwrap().get_mut(&attempt.workflow_id) {
            entry.loaded = Some(session.into());
        }
    }
    pub fn stop(&self, id: &str) {
        if let Some(entry) = self.entries.lock().unwrap().remove(id) {
            entry.client.stop();
        }
    }
    pub fn stop_all(&self) {
        for (_, entry) in self.entries.lock().unwrap().drain() {
            entry.client.stop();
        }
    }
    pub fn remove(&self, id: &str) -> Result<(), String> {
        uuid::Uuid::parse_str(id).map_err(|_| "协作标识无效")?;
        self.stop(id);
        let target = self.root.join(id);
        if !target.exists() {
            return Ok(());
        }
        let root = self.root.canonicalize().map_err(|_| "管家缓存目录不可用")?;
        let target = target.canonicalize().map_err(|_| "管家缓存目录不可用")?;
        if target == root || !target.starts_with(root) {
            return Err("管家缓存目录越界".into());
        }
        std::fs::remove_dir_all(target).map_err(|_| "管家缓存清理失败".into())
    }
}
pub fn manager_directory(db: &Path, workflow: &str, agent: &str) -> Result<PathBuf, String> {
    uuid::Uuid::parse_str(workflow).map_err(|_| "协作标识无效")?;
    if !crate::project_store::EXECUTOR_AGENTS.contains(&agent) {
        return Err("管家成员无效".into());
    }
    Ok(db
        .parent()
        .ok_or("数据目录不可用")?
        .join("coordinators")
        .join(workflow)
        .join(agent)
        .join("workspace"))
}
