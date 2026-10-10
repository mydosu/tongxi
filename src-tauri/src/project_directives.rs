//! User instructions posted after a project starts remain part of that workflow.
use crate::project_store::{Roles, Workflow};
use crate::store::Store;
use rusqlite::params;
use serde::Serialize;

type Result<T> = std::result::Result<T, String>;

#[derive(Serialize)]
pub struct UserUpdate {
    pub id: String,
    pub content: String,
}

fn assigned(request: &str, markers: &[&str]) -> Option<String> {
    let mut result = None;
    for clause in request.split(['\n', '，', '、', ',', '。', '；', ';']) {
        let text = clause.to_lowercase();
        if text.contains(['?', '？'])
            && ![
                "让", "请", "由", "交给", "改为", "改成", "负责", "指定", "安排",
            ]
            .iter()
            .any(|verb| text.contains(verb))
        {
            continue;
        }

        let agents = [
            ("codex", "codex-win"),
            ("hermes", "hermes-win"),
            ("dsh", "dsh-win"),
            ("deepseek harness", "dsh-win"),
        ]
        .into_iter()
        .flat_map(|(label, id)| text.match_indices(label).map(move |(at, _)| (at, id)))
        .collect::<Vec<_>>();
        let mut positions = markers
            .iter()
            .flat_map(|marker| text.match_indices(marker).map(|(at, _)| at))
            .collect::<Vec<_>>();
        positions.sort_unstable();
        positions.dedup();
        for at in positions {
            // A sentence can name several agents: bind the closest preceding name,
            // or the following name in constructions such as "执行交给 DSH".
            if let Some((_, agent)) = agents
                .iter()
                .filter(|(pos, _)| *pos <= at)
                .max_by_key(|(pos, _)| *pos)
                .or_else(|| {
                    agents
                        .iter()
                        .filter(|(pos, _)| *pos > at)
                        .min_by_key(|(pos, _)| *pos)
                })
            {
                result = Some((*agent).to_owned());
            }
        }
    }
    result
}

pub fn executor(request: &str) -> Option<String> {
    assigned(
        request,
        &["执行", "实现", "编码", "开发", "修复", "implement"],
    )
}

pub fn apply(request: &str, roles: &mut Roles) {
    for (role, markers) in [
        (&mut roles.plan, &["规划", "方案", "计划", "plan"][..]),
        (
            &mut roles.implement,
            &["执行", "实现", "编码", "开发", "修复", "implement"][..],
        ),
        (
            &mut roles.review,
            &["验收", "复核", "检查", "审核", "测试", "review"][..],
        ),
    ] {
        if let Some(agent) = assigned(request, markers) {
            if role.agent != agent {
                role.agent = agent;
                role.model = None;
                role.effort = None;
            }
        }
    }
}

impl Store {
    pub fn project_user_updates(&self, workflow: &str) -> Result<Vec<UserUpdate>> {
        let mut query = self.connection.prepare("SELECT id,content FROM (SELECT id,content,created_at AS at,rowid AS seq FROM project_team_messages WHERE workflow_id=?1 AND message_kind='user' AND recipient_agent IS NULL UNION ALL SELECT 'answer:'||id AS id,answer AS content,answered_at AS at,rowid AS seq FROM project_questions WHERE workflow_id=?1 AND status='answered') ORDER BY at,seq,id").map_err(|_|"补充需求读取失败")?;
        let result = query
            .query_map([workflow], |row| {
                Ok(UserUpdate {
                    id: row.get(0)?,
                    content: row.get(1)?,
                })
            })
            .map_err(|_| "补充需求读取失败")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "补充需求读取失败".into());
        result
    }

    pub fn project_explicit_executor(&self, workflow: &Workflow) -> Result<Option<String>> {
        let mut choice = executor(&workflow.request);
        for update in self.project_user_updates(&workflow.id)? {
            if let Some(agent) = executor(&update.content) {
                choice = Some(agent);
            }
        }
        Ok(choice)
    }
}

/// Resolve the new roles before atomically storing the message and its effects.
pub fn encoded_roles(store: &Store, workflow: &str, content: &str) -> Result<String> {
    let job = store.workflow(workflow)?;
    let mut roles = crate::project_store::roles_of(&job)?;
    apply(content, &mut roles);
    roles.validate()?;
    let members = store.conversation(&job.conversation_id)?.members;
    if [
        &roles.plan.agent,
        &roles.implement.agent,
        &roles.review.agent,
    ]
    .iter()
    .any(|agent| !members.contains(agent))
    {
        return Err("分工成员必须属于当前项目群".into());
    }
    serde_json::to_string(&roles).map_err(|_| "补充分工保存失败".into())
}

pub fn save_roles(
    tx: &rusqlite::Transaction<'_>,
    workflow: &str,
    roles: &str,
    timestamp: i64,
) -> Result<()> {
    tx.execute(
        "UPDATE workflows SET roles=?1,updated_at=?2 WHERE id=?3",
        params![roles, timestamp, workflow],
    )
    .map_err(|_| "补充分工保存失败")?;
    Ok(())
}
