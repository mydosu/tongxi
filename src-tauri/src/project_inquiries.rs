//! 协作问答：调度要你确认的事挂在项目的问答面板上，不刷进群聊、不挡着协作。
//! 问题挂在项目群（conversation）上而不是某一次协作上：这次没答，下次协作接着挂着；同一件事按 key 去重，答过的不再问。
//! 能回退的事调度先按 default_choice 推进，不可逆的（reversible=false）先搁着；你的回答作为用户原话交给调度，可以当授权引用。
use crate::store::{now, Store};
use rusqlite::{params, Transaction};
use serde::{Deserialize, Serialize};

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, Serialize)]
pub struct Inquiry {
    pub id: String,
    pub conversation_id: String,
    pub workflow_id: Option<String>,
    pub key: String,
    pub asked_by: String,
    pub question: String,
    pub background: String,
    pub options: Vec<String>,
    pub default_choice: Option<String>,
    pub reversible: bool,
    pub status: String,
    pub answer: Option<String>,
    pub seen: bool,
    pub created_at: i64,
    pub answered_at: Option<i64>,
}

/// 调度挂问题时给的内容。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    pub key: String,
    pub question: String,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
    /// 回答之前先按哪个做（可回退的才会先做）。
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default = "yes")]
    pub reversible: bool,
}
fn yes() -> bool {
    true
}

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_inquiries (
          id TEXT PRIMARY KEY,conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,workflow_id TEXT,
          key TEXT NOT NULL,asked_by TEXT NOT NULL,question TEXT NOT NULL,background TEXT NOT NULL DEFAULT '',options TEXT NOT NULL DEFAULT '[]',
          default_choice TEXT,reversible INTEGER NOT NULL DEFAULT 1,status TEXT NOT NULL,answer TEXT,seen INTEGER NOT NULL DEFAULT 0,
          created_at INTEGER NOT NULL,answered_at INTEGER,UNIQUE(conversation_id,key));",
    )
    .map_err(|_| "问答结构迁移失败".into())
}

/// 校验调度给的问题：每项一句清楚的问题，长度有限。
pub fn check(asks: &[Ask]) -> Result<()> {
    if asks.len() > 12 {
        return Err("一回合最多挂 12 个问题".into());
    }
    for ask in asks {
        let key = ask.key.trim();
        if key.is_empty() || key.chars().count() > 60 {
            return Err("每个问题要有 key（60 字以内，同一件事用同一个 key）".into());
        }
        if ask.question.trim().is_empty() || ask.question.chars().count() > 400 {
            return Err("问题写成一句清楚的话（400 字以内）".into());
        }
        if ask.background.as_ref().is_some_and(|text| text.chars().count() > 1500)
            || ask.default.as_ref().is_some_and(|text| text.chars().count() > 300)
            || ask.options.len() > 6
            || ask.options.iter().any(|option| option.trim().is_empty() || option.chars().count() > 200)
        {
            return Err("问题背景 1500 字以内、默认做法 300 字以内、选项最多 6 个且每个 200 字以内".into());
        }
    }
    Ok(())
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Inquiry> {
    Ok(Inquiry {
        id: r.get(0)?,
        conversation_id: r.get(1)?,
        workflow_id: r.get(2)?,
        key: r.get(3)?,
        asked_by: r.get(4)?,
        question: r.get(5)?,
        background: r.get(6)?,
        options: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
        default_choice: r.get(8)?,
        reversible: r.get(9)?,
        status: r.get(10)?,
        answer: r.get(11)?,
        seen: r.get(12)?,
        created_at: r.get(13)?,
        answered_at: r.get(14)?,
    })
}
const COLUMNS: &str = "id,conversation_id,workflow_id,key,asked_by,question,background,options,default_choice,reversible,status,answer,seen,created_at,answered_at";

impl Store {
    /// 面板用：没答的全部，加最近答过的 30 条。
    pub fn project_inquiries(&self, room: &str) -> Result<Vec<Inquiry>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM project_inquiries WHERE conversation_id=?1 AND (status='open' OR id IN (SELECT id FROM project_inquiries WHERE conversation_id=?1 AND status='answered' ORDER BY answered_at DESC LIMIT 30)) ORDER BY status='answered',created_at"
        );
        let mut query = self.connection.prepare(&sql).map_err(|_| "问答读取失败")?;
        let rows = query.query_map([room], row).map_err(|_| "问答读取失败")?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(|_| "问答读取失败".into())
    }

    /// 挂问题。同一个 key 已经答过就不再问（返回 false）；还没答的更新内容；撤下过的重新挂出来。
    pub fn ask_inquiry(&self, room: &str, workflow: Option<&str>, asked_by: &str, ask: &Ask) -> Result<bool> {
        let key = ask.key.trim();
        let existing: Option<String> = self
            .connection
            .query_row("SELECT status FROM project_inquiries WHERE conversation_id=?1 AND key=?2", params![room, key], |r| r.get(0))
            .ok();
        let options = serde_json::to_string(&ask.options).unwrap();
        let background = ask.background.as_deref().unwrap_or_default().trim();
        match existing.as_deref() {
            Some("answered") => Ok(false),
            Some(status) => {
                self.connection
                    .execute(
                        "UPDATE project_inquiries SET question=?1,background=?2,options=?3,default_choice=?4,reversible=?5,status='open',workflow_id=COALESCE(?6,workflow_id) WHERE conversation_id=?7 AND key=?8",
                        params![ask.question.trim(), background, options, ask.default, ask.reversible, workflow, room, key],
                    )
                    .map_err(|_| "问题更新失败")?;
                Ok(status == "withdrawn")
            }
            None => {
                self.connection
                    .execute(
                        "INSERT INTO project_inquiries(id,conversation_id,workflow_id,key,asked_by,question,background,options,default_choice,reversible,status,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'open',?11)",
                        params![uuid::Uuid::new_v4().to_string(), room, workflow, key, asked_by, ask.question.trim(), background, options, ask.default, ask.reversible, now()],
                    )
                    .map_err(|_| "问题保存失败")?;
                Ok(true)
            }
        }
    }

    /// 撤下没答的问题（事情已经没意义了）。
    pub fn withdraw_inquiry(&self, room: &str, key: &str) -> Result<()> {
        self.connection
            .execute("UPDATE project_inquiries SET status='withdrawn' WHERE conversation_id=?1 AND key=?2 AND status='open'", params![room, key.trim()])
            .map_err(|_| "问题撤下失败")?;
        Ok(())
    }

    pub fn answer_inquiry(&self, room: &str, id: &str, answer: &str) -> Result<Inquiry> {
        let answer = answer.trim();
        if answer.is_empty() || answer.chars().count() > 4000 {
            return Err("回答需为 1–4000 个字".into());
        }
        let changed = self
            .connection
            .execute(
                "UPDATE project_inquiries SET status='answered',answer=?1,answered_at=?2,seen=0 WHERE id=?3 AND conversation_id=?4 AND status='open'",
                params![answer, now(), id, room],
            )
            .map_err(|_| "回答保存失败")?;
        if changed == 0 {
            return Err("这个问题已经答过或被撤下了".into());
        }
        let sql = format!("SELECT {COLUMNS} FROM project_inquiries WHERE id=?1");
        self.connection.query_row(&sql, [id], row).map_err(|_| "回答读取失败".into())
    }

    /// 调度这回合看过了新回答：下回合不再当新的给。
    pub fn mark_inquiries_seen(&self, room: &str) -> Result<()> {
        self.connection
            .execute("UPDATE project_inquiries SET seen=1 WHERE conversation_id=?1 AND status='answered' AND seen=0", [room])
            .map_err(|_| "问答状态保存失败")?;
        Ok(())
    }

    /// 累计答过几个：调度暂停等待时用它判断有没有新回答进来。
    pub fn answered_inquiries(&self, room: &str) -> usize {
        self.connection
            .query_row("SELECT COUNT(*) FROM project_inquiries WHERE conversation_id=?1 AND status='answered'", [room], |r| r.get::<_, i64>(0))
            .map_or(0, |n| n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(key: &str, question: &str) -> Ask {
        Ask { key: key.into(), question: question.into(), background: None, options: vec!["4".into(), "6".into()], default: Some("先用 4".into()), reversible: true }
    }

    #[test]
    fn questions_dedupe_by_key_and_answered_ones_are_not_asked_again() {
        let dir = std::env::temp_dir().join(format!("inquiry-test-{}", uuid::Uuid::new_v4()));
        let mut store = Store::open(&dir.join("hub.db")).unwrap();
        let room = store.create("项目", "group", &["codex-win".into()]).unwrap().id;
        assert!(store.ask_inquiry(&room, None, "coordinator", &ask("tts-version", "生产用哪个情感版本？")).unwrap());
        assert!(!store.ask_inquiry(&room, None, "coordinator", &ask("tts-version", "生产 TTS 情感版本用 4 还是 6？")).unwrap());
        let open = store.project_inquiries(&room).unwrap();
        assert_eq!((open.len(), open[0].question.as_str()), (1, "生产 TTS 情感版本用 4 还是 6？"));
        let answered = store.answer_inquiry(&room, &open[0].id, "用 6").unwrap();
        assert_eq!((answered.status.as_str(), answered.answer.as_deref(), answered.seen), ("answered", Some("用 6"), false));
        assert!(store.answer_inquiry(&room, &open[0].id, "再答一次").is_err());
        // 答过的不再问。
        assert!(!store.ask_inquiry(&room, None, "coordinator", &ask("tts-version", "还是问一下")).unwrap());
        assert_eq!(store.answered_inquiries(&room), 1);
        store.mark_inquiries_seen(&room).unwrap();
        assert!(store.project_inquiries(&room).unwrap()[0].seen);
        // 撤下后再挂出来算新问题。
        store.ask_inquiry(&room, None, "coordinator", &ask("live2d", "Live2D 还要吗？")).unwrap();
        store.withdraw_inquiry(&room, "live2d").unwrap();
        assert!(store.ask_inquiry(&room, None, "coordinator", &ask("live2d", "Live2D 还要吗？")).unwrap());
        assert!(check(&[ask(" ", "x")]).is_err() && check(&[ask("k", "问")]).is_ok());
    }
}
