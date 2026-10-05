use crate::{
    codex, dsh,
    group_store::{self, Discussion},
    hermes,
    store::{RunRecord, Store},
};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tauri::{Emitter, State};

type Result<T> = std::result::Result<T, String>;

pub struct Runtime {
    app: tauri::AppHandle,
    store: Arc<Mutex<Store>>,
    codex: Arc<codex::Runtime>,
    hermes: Arc<hermes::Runtime>,
    dsh: Arc<dsh::Runtime>,
    albion: Arc<crate::albion::Brain>,
    shutting_down: AtomicBool,
    revision: AtomicU64,
}

#[derive(Clone, Serialize)]
pub struct Event {
    pub revision: u64,
    pub discussion: Discussion,
}

impl Runtime {
    pub fn new(
        app: tauri::AppHandle,
        store: Arc<Mutex<Store>>,
        codex: Arc<codex::Runtime>,
        hermes: Arc<hermes::Runtime>,
        dsh: Arc<dsh::Runtime>,
        albion: Arc<crate::albion::Brain>,
    ) -> Arc<Self> {
        Arc::new(Self {
            app,
            store,
            codex,
            hermes,
            dsh,
            albion,
            shutting_down: AtomicBool::new(false),
            revision: AtomicU64::new(0),
        })
    }
    fn emit(&self, id: &str) {
        let store = self.store.lock().unwrap();
        if let Ok(discussion) = store.discussion(id) {
            let event = Event {
                revision: self.revision.fetch_add(1, Ordering::SeqCst) + 1,
                discussion,
            };
            drop(store);
            let _ = self.app.emit("discussion-state", event);
        }
    }
    fn snapshot(&self, agent: &str) -> codex::RuntimeSnapshot {
        match agent {
            "codex-win" => self.codex.snapshot(),
            "hermes-win" => self.hermes.snapshot(),
            "dsh-win" => self.dsh.snapshot(),
            _ => self.albion.snapshot(),
        }
    }
    fn dispatch(&self, id: &str, agent: &str, round: u32) -> Result<RunRecord> {
        match agent {
            "codex-win" => self.codex.send_discussion(id, round),
            "hermes-win" => self.hermes.send_discussion(id, round),
            "dsh-win" => self.dsh.send_discussion(id, round),
            "albion-wsl" => self.albion.send_discussion(id, round),
            _ => Err("成员不存在".into()),
        }
    }
    fn cancel_member(&self, agent: &str, room: &str, run: &str) -> Result<()> {
        match agent {
            "codex-win" => self.codex.cancel_expected(room, Some(run)).map(|_| ()),
            "hermes-win" => self.hermes.cancel_expected(room, Some(run)).map(|_| ()),
            "dsh-win" => self.dsh.cancel_expected(room, Some(run)).map(|_| ()),
            _ => self.albion.cancel_expected(room, Some(run)).map(|_| ()),
        }
    }
    fn stop_member(&self, agent: &str, run: &str) {
        match agent {
            "codex-win" => self.codex.stop_expected(Some(run)),
            "hermes-win" => self.hermes.stop_expected(Some(run)),
            "dsh-win" => self.dsh.stop_expected(Some(run)),
            _ => self.albion.stop_expected(Some(run)),
        }
    }
    pub fn start(
        self: &Arc<Self>,
        room: &str,
        message: &str,
        content: &str,
        participants: &[String],
        rounds: u32,
    ) -> Result<Discussion> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("软件正在关闭".into());
        }
        let existing = self.store.lock().unwrap().discussion_for_message(message)?;
        if existing.is_none() {
            for agent in participants {
                let state = self.snapshot(agent);
                if state.connection != "connected"
                    || state
                        .active
                        .as_ref()
                        .is_some_and(|r| group_store::run_active(&r.record.status))
                {
                    return Err("请先连接所有参与成员，并等待他们当前的回复完成".into());
                }
            }
        }
        let (job, new) = self.store.lock().unwrap().start_discussion(
            room,
            message,
            content,
            participants,
            rounds,
        )?;
        if new {
            self.emit(&job.id);
            let runtime = self.clone();
            let id = job.id.clone();
            std::thread::spawn(move || runtime.work(&id));
        }
        Ok(job)
    }
    fn finish(&self, id: &str, status: &str, error: Option<&str>) {
        let result = self
            .store
            .lock()
            .unwrap()
            .finish_discussion(id, status, error);
        if result.is_ok() {
            self.emit(id);
        }
    }
    fn work(&self, id: &str) {
        let started = Instant::now();
        let mut cancellation = None;
        let mut observed = String::new();
        loop {
            if self.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            let job = match self.store.lock().unwrap().discussion(id) {
                Ok(job) => job,
                Err(_) => return,
            };
            if !group_store::active(&job.status) {
                return;
            }
            let last = job.turns.last();
            if let Some(run) = last.filter(|r| group_store::run_active(&r.status)) {
                if job.status == "cancelling" {
                    let began = cancellation.get_or_insert_with(Instant::now);
                    if began.elapsed() > Duration::from_secs(30) {
                        self.stop_member(&run.agent_id, &run.id);
                    }
                } else if started.elapsed() > Duration::from_secs(900) {
                    let _ = self.store.lock().unwrap().cancel_discussion(id);
                    let _ = self.cancel_member(&run.agent_id, &job.conversation_id, &run.id);
                    self.emit(id);
                }
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            if job.status == "cancelling" {
                self.finish(id, "interrupted", Some("讨论已停止，后续成员和轮次未启动"));
                return;
            }
            if let Some(run) = last {
                if run.id != observed {
                    observed = run.id.clone();
                    self.emit(id);
                }
                if run.status != "completed" {
                    self.finish(
                        id,
                        if run.status == "interrupted" {
                            "interrupted"
                        } else {
                            "failed"
                        },
                        Some(
                            run.error
                                .as_deref()
                                .unwrap_or("成员发言中断，后续讨论未启动"),
                        ),
                    );
                    return;
                }
            }
            if job.turns.len() == job.participants.len() * job.rounds as usize {
                self.finish(id, "completed", None);
                return;
            }
            let index = job.turns.len();
            match self.dispatch(
                id,
                &job.participants[index % job.participants.len()],
                (index / job.participants.len() + 1) as u32,
            ) {
                Ok(_) => self.emit(id),
                Err(error) => {
                    // Cancellation may have won the atomic store admission check.
                    let cancelling = self
                        .store
                        .lock()
                        .unwrap()
                        .discussion(id)
                        .is_ok_and(|j| j.status == "cancelling");
                    self.finish(
                        id,
                        if cancelling { "interrupted" } else { "failed" },
                        Some(&error),
                    );
                    return;
                }
            }
        }
    }
    /// 该成员是否正在参与群讨论发言（活动讨论的参与者含该成员）。供维护锁互斥判断使用。
    pub fn cancel(&self, id: &str) -> Result<Discussion> {
        let job = self.store.lock().unwrap().cancel_discussion(id)?;
        self.emit(id);
        if let Some(run) = job
            .turns
            .iter()
            .find(|r| group_store::run_active(&r.status))
        {
            if self
                .cancel_member(&run.agent_id, &job.conversation_id, &run.id)
                .is_err()
            {
                self.stop_member(&run.agent_id, &run.id);
            }
        }
        self.store.lock().unwrap().discussion(id)
    }
    pub fn stop(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }
}

#[tauri::command]
pub fn discussion_status(runtime: State<'_, Arc<Runtime>>) -> Result<Option<Discussion>> {
    runtime.store.lock().unwrap().active_discussion()
}
#[tauri::command]
pub fn start_discussion(
    runtime: State<'_, Arc<Runtime>>,
    conversation_id: String,
    message_id: String,
    content: String,
    participants: Vec<String>,
    rounds: u32,
) -> Result<Discussion> {
    runtime.start(
        &conversation_id,
        &message_id,
        &content,
        &participants,
        rounds,
    )
}
#[tauri::command]
pub async fn cancel_discussion(runtime: State<'_, Arc<Runtime>>, id: String) -> Result<Discussion> {
    let runtime = runtime.inner().clone();
    tauri::async_runtime::spawn_blocking(move || runtime.cancel(&id))
        .await
        .map_err(|_| "停止讨论任务异常".to_string())?
}
