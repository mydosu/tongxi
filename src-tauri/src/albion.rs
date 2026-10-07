//! 阿尔比恩（WSL，**Pi 脑**）在 Windows 侧的运行时。
//!
//! 本地服务 = `/root/albion-pi/bin/server.py` 提供的 OpenAI 兼容端点（本机 `:8650`）：
//! 人格、回忆、工具、说话守卫都在脑里，同席只当**客户端**——连接=取模型，发送=重发房间历史拿流式正文。
//! 启停归她自己的 `start.sh` / `_启动脑Pi.ps1`：同席**不拉 WSL 进程**（也不再走 ACP）。
//!
//! ponytail: MVP 范围＝连上 + 私聊真流式；群聊/项目把她当成员、线级中断还没接。

use crate::albion_openai;
use crate::codex::{RunSnapshot, RuntimeSnapshot};
use crate::models::ModelOption;
use crate::store::{RunRecord, Store};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, State};

const ID: &str = "albion-wsl";
const KEY: &str = "albion";
const NAME: &str = "Albion";

struct Inner {
    revision: u64,
    connection: String,
    error: Option<String>,
    models: Vec<String>,
    active: Option<RunSnapshot>,
}

/// 这具脑的运行时：连接态 + 取数端点 + 当前回合。
pub struct Brain {
    app: tauri::AppHandle,
    store: Arc<Mutex<Store>>,
    endpoint: String,
    inner: Mutex<Inner>,
}

/// 一个回合还没结束（与 Hermes 侧同一套判定）。
fn busy(run: &RunSnapshot) -> bool {
    matches!(run.record.status.as_str(), "running" | "streaming")
}

impl Brain {
    pub fn new(app: tauri::AppHandle, store: Arc<Mutex<Store>>, _directory: PathBuf) -> Self {
        Self {
            app,
            store,
            endpoint: albion_openai::endpoint(),
            inner: Mutex::new(Inner {
                revision: 0,
                connection: "disconnected".into(),
                error: None,
                models: Vec::new(),
                active: None,
            }),
        }
    }

    fn emit(&self) {
        let _ = self.app.emit(&format!("{KEY}-state"), self.snapshot());
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        let inner = self.inner.lock().unwrap();
        RuntimeSnapshot {
            revision: inner.revision,
            connection: inner.connection.clone(),
            executable: Some(self.endpoint.clone()),
            version: Some("pi".into()),
            error: inner.error.clone(),
            active: inner.active.clone(),
            models: inner
                .models
                .iter()
                .map(|id| ModelOption {
                    id: id.clone(),
                    name: id.clone(),
                    provider_id: None,
                    provider_name: None,
                    efforts: Vec::new(),
                    default_effort: None,
                })
                .collect(),
            default_model: inner.models.first().cloned(),
            default_effort: None,
        }
    }

    /// 连接 = 取一次模型目录；她没起来就明确报错，不假装连上。
    pub fn connect(&self) -> Result<RuntimeSnapshot, String> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.revision += 1;
            inner.connection = "connecting".into();
            inner.error = None;
        }
        self.emit();
        let result = albion_openai::models(&self.endpoint);
        {
            let mut inner = self.inner.lock().unwrap();
            inner.revision += 1;
            match result {
                Ok(models) => {
                    inner.connection = "connected".into();
                    inner.models = models;
                }
                Err(error) => {
                    inner.connection = "disconnected".into();
                    inner.error = Some(error);
                }
            }
        }
        self.emit();
        let snapshot = self.snapshot();
        match snapshot.connection.as_str() {
            "connected" => Ok(snapshot),
            _ => Err(snapshot.error.unwrap_or_else(|| format!("连不上 {NAME}"))),
        }
    }

    pub fn stop(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.revision += 1;
            inner.connection = "disconnected".into();
            inner.active = None;
        }
        self.emit();
    }

    /// 一轮私聊：把房间历史重发给她（她每轮重发历史，所以重载不失忆），正文边收边推进快照。
    pub fn send(
        self: &Arc<Self>,
        conversation_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<RunRecord, String> {
        let record = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(existing) = self.store.lock().unwrap().run_for_message(message_id)? {
                self.store
                    .lock()
                    .unwrap()
                    .save_message(conversation_id, message_id, content)?;
                return Ok(existing);
            }
            if inner.connection != "connected" {
                return Err(format!("请先连接 {NAME}"));
            }
            if inner.active.as_ref().is_some_and(busy) {
                return Err(format!("{NAME} 正在处理上一条消息，请等待或停止"));
            }
            // 人这一条也由 begin_agent_run 落库，之后 history() 会把它一起重发。
            let record = self.store.lock().unwrap().begin_agent_run(
                conversation_id,
                message_id,
                content,
                ID,
            )?;
            inner.active = Some(RunSnapshot {
                record: record.clone(),
                text: String::new(),
                thought: String::new(),
            });
            record
        };
        self.emit();
        let runtime = Arc::clone(self);
        let run_id = record.id.clone();
        let conversation = conversation_id.to_string();
        std::thread::spawn(move || runtime.turn(&conversation, &run_id));
        Ok(record)
    }

    fn turn(self: &Arc<Self>, conversation_id: &str, run_id: &str) {
        let messages = match self.history(conversation_id) {
            Ok(messages) => messages,
            Err(error) => return self.finish(run_id, "failed", Some(error)),
        };
        let runtime = Arc::clone(self);
        let result = albion_openai::chat(&self.endpoint, &messages, |delta| {
            {
                let mut inner = runtime.inner.lock().unwrap();
                if let Some(run) = inner.active.as_mut().filter(|run| run.record.id == run_id) {
                    run.text.push_str(delta);
                }
            }
            runtime.emit();
        });
        match result {
            Ok(text) => {
                // 正文以最终返回为准（增量里含 <|ACT:…|> 标记，finish 里统一剥）。
                {
                    let mut inner = self.inner.lock().unwrap();
                    if let Some(run) = inner.active.as_mut().filter(|run| run.record.id == run_id) {
                        run.text = text;
                    }
                }
                self.finish(run_id, "succeeded", None);
            }
            Err(error) => self.finish(run_id, "failed", Some(error)),
        }
    }

    /// 房间历史 → OpenAI messages（人 = `sender_id == "user"`，其余当助手）。
    fn history(&self, conversation_id: &str) -> Result<serde_json::Value, String> {
        let detail = self.store.lock().unwrap().detail(conversation_id)?;
        let messages: Vec<serde_json::Value> = detail
            .messages
            .iter()
            .filter(|message| message.status != "failed" && !message.content.trim().is_empty())
            .map(|message| {
                let role = if message.sender_id == "user" {
                    "user"
                } else {
                    "assistant"
                };
                serde_json::json!({ "role": role, "content": message.content })
            })
            .collect();
        Ok(serde_json::Value::Array(messages))
    }

    fn finish(&self, id: &str, status: &str, error: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(run) = inner.active.as_mut().filter(|run| run.record.id == id) {
            run.record.status = status.into();
            run.record.error = error;
            // 显示层剥掉 <|ACT:…|> 情绪标记（与 Hermes 时代同一条路）。
            run.text = crate::chat_text::albion_complete(&run.text);
            let _ = self
                .store
                .lock()
                .unwrap()
                .checkpoint(&run.record, &run.text, &run.thought);
        }
        drop(inner);
        self.emit();
    }

    /// 群聊/项目里把她当成员：MVP 先明确拒绝（这条通路先只做私聊）。
    pub(crate) fn send_discussion(
        &self,
        discussion: &str,
        round: u32,
    ) -> Result<RunRecord, String> {
        let _ = (discussion, round);
        Err(format!("{NAME} 这条通路先只做私聊，群聊里还叫不上她"))
    }

    /// 群聊取消：她本来没跑过群聊回合，按实际状态如实回。
    pub(crate) fn cancel_expected(
        &self,
        conversation_id: &str,
        expected: Option<&str>,
    ) -> Result<RuntimeSnapshot, String> {
        let _ = expected;
        self.cancel(conversation_id)
    }

    pub(crate) fn stop_expected(&self, expected: Option<&str>) {
        let _ = expected;
        self.stop();
    }

    /// 归档/删除/改成员前的互斥钩子：她这条通路没有 ACP 客户端要护，直接放行。
    pub fn guard<T>(
        &self,
        id: &str,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let _ = id;
        action()
    }

    /// 她的会话库在她脑自己的 state.db 里（不在同席这边），MVP 如实回空表。
    pub fn list_sessions(&self) -> Result<Vec<crate::codex::NativeSession>, String> {
        Ok(Vec::new())
    }

    /// MVP：不做线级中断，只把她这一轮标记为已取消（脑里的回合会自己跑完）。
    pub fn cancel(&self, conversation_id: &str) -> Result<RuntimeSnapshot, String> {
        let mut inner = self.inner.lock().unwrap();
        let run = inner
            .active
            .as_mut()
            .filter(|run| run.record.conversation_id == conversation_id && busy(run))
            .ok_or(format!("{NAME} 当前没有在回复"))?;
        run.record.status = "cancelled".into();
        let record = run.record.clone();
        let (text, thought) = (run.text.clone(), run.thought.clone());
        drop(inner);
        self.store
            .lock()
            .unwrap()
            .checkpoint(&record, &text, &thought)?;
        self.emit();
        Ok(self.snapshot())
    }
}

/// 状态身份保持 `.0`：`services.rs` 等处按这个字段取运行时。
pub struct Runtime(pub Arc<Brain>);
impl Runtime {
    pub fn new(app: tauri::AppHandle, store: Arc<Mutex<Store>>, directory: PathBuf) -> Self {
        Self(Arc::new(Brain::new(app, store, directory)))
    }
}

#[tauri::command]
pub fn albion_status(runtime: State<'_, Runtime>) -> RuntimeSnapshot {
    runtime.0.snapshot()
}
#[tauri::command]
pub async fn connect_albion(
    app: tauri::AppHandle,
    runtime: State<'_, Runtime>,
    lock: State<'_, crate::services::MaintenanceLock>,
) -> Result<RuntimeSnapshot, String> {
    // 连接维护互斥：guard 覆盖整个 spawn_blocking 连接过程，与维护/重启串行。
    let _guard = crate::services::acquire(&app, lock.inner(), "albion-wsl", "connect")?;
    let inner = runtime.0.clone();
    tauri::async_runtime::spawn_blocking(move || inner.connect())
        .await
        .map_err(|_| "阿尔比恩连接任务异常".to_string())?
}
#[tauri::command]
pub async fn disconnect_albion(
    app: tauri::AppHandle,
    runtime: State<'_, Runtime>,
    lock: State<'_, crate::services::MaintenanceLock>,
) -> Result<RuntimeSnapshot, String> {
    // store 原子 shutdown 标记挡群聊/项目、放行 privateRun；LockGuard 到命令结束才清。
    let _guard = crate::services::acquire_shutdown(&app, lock.inner(), "albion-wsl")?;
    let runtime = runtime.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        runtime.stop();
        runtime.snapshot()
    })
    .await
    .map_err(|_| "阿尔比恩停止任务异常".to_string())
}
#[tauri::command]
pub fn send_albion_message(
    runtime: State<'_, Runtime>,
    conversation_id: String,
    message_id: String,
    content: String,
) -> Result<RunRecord, String> {
    runtime.0.send(&conversation_id, &message_id, &content)
}
#[tauri::command]
pub fn cancel_albion_run(
    runtime: State<'_, Runtime>,
    conversation_id: String,
) -> Result<RuntimeSnapshot, String> {
    runtime.0.cancel(&conversation_id)
}
