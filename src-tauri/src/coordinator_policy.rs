//! 调度主脑：按用户原话锚定任务，自己写任务说明、按任务选模型与强度；用户点名的一律照办。
use crate::{models::ModelOption, project_store::RoleChoice};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MANAGER_ACTIVITY: &str = "coordinate";
/// 管家所在成员处理原仓库合并/变基中途状态的回合。
pub const GIT_REPAIR_ACTIVITY: &str = "resolve:git";
/// 调度和成员的说明放在外置文件里，改了下一回合就生效，不用重新打包重启：
/// 先找数据目录下的 prompts/（用户自己改的版本），调试版再找源码里的 prompts/，都没有就用打包进来的那份。
const MANAGER_DEFAULT: &str = include_str!("../prompts/manager.md");
const MEMBER_DEFAULT: &str = include_str!("../prompts/member.md");
static PROMPT_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_prompt_dir(directory: std::path::PathBuf) {
    let _ = PROMPT_DIR.set(directory);
}

/// 依次可能存放某份说明的外置文件，排在前面的优先。
pub fn prompt_files(name: &str) -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = PROMPT_DIR.get().map(|dir| dir.join(name)).into_iter().collect();
    if cfg!(debug_assertions) {
        files.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts").join(name));
    }
    files
}

fn load_prompt(name: &str, fallback: &str) -> String {
    prompt_files(name)
        .iter()
        .find_map(|file| std::fs::read_to_string(file).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| fallback.trim().to_owned())
}

pub fn manager_instruction() -> String {
    load_prompt("manager.md", MANAGER_DEFAULT)
}

pub fn member_instruction() -> String {
    load_prompt("member.md", MEMBER_DEFAULT)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub source_id: String,
    pub quote: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub key: String,
    #[serde(default)]
    pub task_id: Option<String>,
    pub purpose: String,
    pub agent_id: String,
    pub source_id: String,
    pub quote: String,
    #[serde(default)]
    pub context_ids: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub model_source: Option<Reference>,
    /// 子代理验证：交给改动源文件的成员，由它用自带的子代理工具派低价模型、高思考强度的子代理验证。
    #[serde(default)]
    pub subagent: bool,
    /// 调度写的任务说明：目标、产出、资料、边界。与用户原话冲突时以原话为准。
    #[serde(default)]
    pub instructions: Option<String>,
    /// 一句话理由：为什么派这位成员、选这个模型和强度。
    #[serde(default)]
    pub reason: Option<String>,
    /// 新开一个会话（从零上下文开始），之后和用户建的会话一样留在项目里。
    #[serde(default)]
    pub fresh_session: bool,
    /// 成员在原来的会话位置从零上下文开始做这项（旧会话留在原生历史里），不占新开会话的名额。
    #[serde(default)]
    pub clear_context: bool,
    /// 允许成员在本任务里用自带的子 agent。
    #[serde(default)]
    pub allow_subagents: bool,
    /// 任务名：进度里显示，写这项具体做什么。
    #[serde(default)]
    pub title: Option<String>,
    /// 任务类别（capability::CATEGORIES）与复杂度：归档进能力画像，下次选型时按类别看历史表现。
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub complexity: Option<String>,
    /// 合并前须验证：任务完成后先不并入主线，等引用它的验证任务写明通过才合并。
    #[serde(default)]
    pub hold_merge: bool,
    /// 软预算：预计这项的总 token。成员超了先停下交进度报告，由调度决定接着怎么办。
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// 请教模式：成员这项任务里最多能向调度请教几次（hub_ask_advisor），不写或 0 就不开放。
    #[serde(default)]
    pub advisor_calls: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub prompt: String,
    #[serde(default)]
    pub options: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub question: String,
    pub choice: String,
    #[serde(default)]
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Withdrawal {
    pub task_id: String,
    pub source_id: String,
    pub quote: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub action: String,
    #[serde(default)]
    pub withdraw: Vec<Withdrawal>,
    #[serde(default)]
    pub routes: Vec<Route>,
    /// 调度自己拍板的取舍：发到群里告知用户，不等回答。
    #[serde(default)]
    pub decisions: Vec<Choice>,
    #[serde(default)]
    pub question: Option<Question>,
    #[serde(default)]
    pub result: Option<Reference>,
    /// 计划账本：目标、步骤与状态、关键决定、风险。下一回合原样交还调度。
    #[serde(default)]
    pub ledger: Option<String>,
    /// 项目长期计划（.tongxi/计划.md）的完整新版；没变就省略。
    #[serde(default)]
    pub plan: Option<String>,
    /// 挂到问答面板的问题（按 key 去重，答过的不再问）。
    #[serde(default)]
    pub inquiries: Vec<crate::project_inquiries::Ask>,
    /// 撤下已经没意义的问题（key）。
    #[serde(default)]
    pub withdraw_inquiries: Vec<String>,
    /// 分板块推进时 finish 带上剩下的板块，发到群里让用户挑哪天做哪块。
    #[serde(default)]
    pub next_sections: Vec<Section>,
    /// 额度或余额撑不下去时主动暂停：说明原因；给了 resume_in_hours 就到点自动继续，不给就停下等用户说继续。
    #[serde(default)]
    pub pause: Option<Pause>,
    /// 把用户新说的话（或调度的调整）转给正在运行的某项任务；同席不再把群聊自动群发给成员。
    #[serde(default)]
    pub notes: Vec<Note>,
    /// finish 时的遗留账：本场新增、清掉、池里还剩多少条，同席附在收工消息里。
    #[serde(default)]
    pub leftovers: Option<Leftovers>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Note {
    pub task_id: String,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Leftovers {
    pub added: u32,
    pub cleared: u32,
    pub remaining: u32,
}
/// 拨款：一项任务用量超了软预算、成员还在干活时，调度只回这一项。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub action: String,
    #[serde(default)]
    pub extra_tokens: Option<u64>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// 请教：成员在任务里问到的关键决定，调度只回这一项。escalate=true 表示建议换更强的模型接手。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Advice {
    pub answer: String,
    #[serde(default)]
    pub escalate: bool,
}

pub fn parse_advice(raw: &str) -> Result<Advice, String> {
    #[derive(Deserialize)]
    struct Reply {
        advice: Advice,
    }
    let document = crate::project_store::json_document(raw, 16_000)?;
    let advice = serde_json::from_str::<Reply>(document).map_err(|_| "请教回复格式无效".to_owned())?.advice;
    if advice.answer.trim().is_empty() {
        return Err("请教回复为空".into());
    }
    Ok(advice)
}

pub fn parse_grant(raw: &str) -> Result<Grant, String> {
    #[derive(Deserialize)]
    struct Reply {
        grant: Grant,
    }
    let document = crate::project_store::json_document(raw, 8_000)?;
    let grant = serde_json::from_str::<Reply>(document).map_err(|_| "拨款回复格式无效".to_owned())?.grant;
    if !matches!(grant.action.as_str(), "continue" | "wrap_up" | "stop") {
        return Err("拨款动作无效".into());
    }
    Ok(grant)
}

/// 任务失败后当场问调度：retry 就在本轮换人、换模型或补说明重做，later 留到下一回合。
#[derive(Clone, Debug, Deserialize)]
pub struct Redo {
    pub action: String,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

pub fn parse_redo(raw: &str) -> Result<Redo, String> {
    #[derive(Deserialize)]
    struct Reply {
        redo: Redo,
    }
    let document = crate::project_store::json_document(raw, 8_000)?;
    let redo = serde_json::from_str::<Reply>(document).map_err(|_| "重派回复格式无效".to_owned())?.redo;
    if !matches!(redo.action.as_str(), "retry" | "later") {
        return Err("重派动作无效".into());
    }
    Ok(redo)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pause {
    pub reason: String,
    #[serde(default)]
    pub resume_in_hours: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Source {
    pub id: String,
    pub kind: String,
    pub text: String,
}

/// 板块标题去掉调度沿用的旧序号（「3. 」「3、」「(3)」「第3块：」），群里统一从 1 重新编号。
pub fn section_title(title: &str) -> &str {
    let text = title.trim();
    let rest = text.strip_prefix('第').unwrap_or(text);
    let rest = rest.strip_prefix(['(', '（']).unwrap_or(rest);
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return text;
    }
    let after = &rest[digits..];
    let after = after.strip_prefix(['块', '项']).unwrap_or(after);
    match after.strip_prefix(['.', '、', ')', '）', ':', '：']) {
        Some(tail) if !tail.trim().is_empty() => tail.trim_start_matches(['.', ' ', ':', '：']).trim(),
        _ => text,
    }
}

/// 长期授权里写了允许推送、又没写不许推时，同席合并任务分支后自动推送（「不 force push」不算不许推）。
pub fn grants_push(grants: &str) -> bool {
    let text: String = grants.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    let denied = ["不push", "不要push", "不许push", "不允许push", "禁止push", "不推送", "不要推送", "不许推送", "禁止推送"];
    (text.contains("push") || text.contains("推送")) && !denied.iter().any(|word| text.contains(word))
}

pub fn parse(raw: &str) -> Result<Decision, String> {
    let document = crate::project_store::json_document(raw, 64_000)?;
    let mut value: Decision = serde_json::from_str(document)
        .map_err(|_| "调度返回的 JSON 格式无效或带了未知字段".to_owned())?;
    // 调度常把同批前置任务的 key 写进 context_ids：那时还没有结果可引用，按依赖处理。
    let keys: Vec<String> = value.routes.iter().map(|route| route.key.clone()).collect();
    for route in &mut value.routes {
        let (siblings, results): (Vec<_>, Vec<_>) =
            route.context_ids.drain(..).partition(|id| keys.contains(id) && *id != route.key);
        route.context_ids = results;
        for key in siblings {
            if !route.depends_on.contains(&key) {
                route.depends_on.push(key);
            }
        }
    }
    if !matches!(
        value.action.as_str(),
        "delegate" | "ask_user" | "wait" | "finish" | "pause"
    ) || value.routes.len() > 32
    {
        return Err("管家需返回可执行的路由动作".into());
    }
    if value.withdraw.len() > 32 {
        return Err("每回合撤回任务过多".into());
    }
    if value.decisions.len() > 20
        || value.decisions.iter().any(|item| {
            item.question.trim().is_empty()
                || item.choice.trim().is_empty()
                || [Some(&item.question), Some(&item.choice), item.reason.as_ref()]
                    .into_iter()
                    .flatten()
                    .any(|text| text.chars().count() > 300)
        })
    {
        return Err("decisions 每项写一句问题和你的选择（各300字以内），最多20项".into());
    }
    if value.action == "ask_user" && !value.decisions.is_empty() {
        return Err("提问时不要同时拍板；能自己定的先 delegate 推进".into());
    }
    if value.next_sections.len() > 20
        || value.next_sections.iter().any(|section| {
            section.title.trim().is_empty()
                || section.title.chars().count() > 40
                || section.summary.as_ref().is_some_and(|summary| summary.chars().count() > 200)
        })
    {
        return Err("next_sections 最多 20 项，title 不能为空且不超过40字，summary 不超过200字".into());
    }
    if !value.next_sections.is_empty() && value.action != "finish" {
        return Err("next_sections 只在 finish 时使用".into());
    }
    crate::project_inquiries::check(&value.inquiries)?;
    if value.plan.as_ref().is_some_and(|plan| plan.chars().count() > crate::project_plan::LIMIT) {
        return Err("长期计划过长，请压缩到 12000 字以内，细节放进方案存档按路径引用".into());
    }
    if value.ledger.as_ref().is_some_and(|ledger| ledger.chars().count() > 3000) {
        return Err("计划账本过长，请压缩到1500字以内".into());
    }
    for route in &value.routes {
        if route.instructions.as_ref().is_some_and(|text| text.chars().count() > 4000) {
            return Err("任务说明过长，请控制在2000字以内，细节通过 context_ids 引用".into());
        }
        if route.reason.as_ref().is_some_and(|text| text.chars().count() > 300) {
            return Err("理由只写一句话".into());
        }
        if route.title.as_ref().is_some_and(|text| text.chars().count() > 40) {
            return Err("任务名控制在20字左右".into());
        }
        if route.advisor_calls.is_some_and(|calls| calls > 5) {
            return Err("advisor_calls 每项任务最多 5 次".into());
        }
        if route.fresh_session && route.subagent {
            return Err("子代理验证由改动者本人派出，不能新开会话".into());
        }
    }
    if value.action != "delegate" && !value.routes.is_empty() {
        return Err("此动作不能同时追加任务".into());
    }
    if value.action == "ask_user"
        && value.question.as_ref().is_none_or(|q| {
            q.prompt.trim().is_empty()
                || q.prompt.chars().count() > 4000
                || q.options.len() > 8
                || q.options
                    .iter()
                    .any(|s| s.trim().is_empty() || s.chars().count() > 500)
        })
    {
        return Err("请提供清楚的问题与选项".into());
    }
    if value.action != "ask_user" && value.question.is_some() {
        return Err("问题只能通过提问动作发布".into());
    }
    match (&value.pause, value.action == "pause") {
        (None, true) => return Err("暂停要写 pause.reason（为什么暂停），能估出额度何时恢复就给 resume_in_hours".into()),
        (Some(_), false) => return Err("pause 只在 action=pause 时使用".into()),
        (Some(pause), true)
            if pause.reason.trim().is_empty()
                || pause.reason.chars().count() > 300
                || pause.resume_in_hours.is_some_and(|hours| !(hours > 0.0 && hours <= 168.0)) =>
        {
            return Err("pause.reason 写一句原因（300字以内），resume_in_hours 在 0 到 168 之间".into())
        }
        _ => {}
    }
    Ok(value)
}
pub fn source<'a>(
    sources: &'a [Source],
    reference: &Reference,
    user_only: bool,
) -> Result<&'a Source, String> {
    let source = sources
        .iter()
        .find(|s| s.id == reference.source_id)
        .ok_or("引用的原文不存在或属于其他协作")?;
    if user_only && source.kind != "user" {
        return Err("任务范围与模型授权必须来自用户原话".into());
    }
    if reference.quote.trim().is_empty() || !source.text.contains(&reference.quote) {
        return Err("任务引用必须保持原文，不得追加或改写指示".into());
    }
    Ok(source)
}
fn clause<'a>(text: &'a str, quote: &str) -> &'a str {
    let Some(start) = text.find(quote) else {
        return text;
    };
    let end = start + quote.len();
    let left = text[..start]
        .rfind(['，', ',', '。', '；', ';', '\n'])
        .map_or(0, |at| at + text[at..].chars().next().unwrap().len_utf8());
    let right = text[end..]
        .find(['，', ',', '。', '；', ';', '\n'])
        .map_or(text.len(), |at| end + at);
    &text[left..right]
}
#[cfg(test)]
pub fn check_assignment(route: &Route, sources: &[Source]) -> Result<(), String> {
    check_assignment_with(route, sources, &[])
}

/// aliases：用户起的成员名（项目里的成员名、用户建的会话名）→ 底层成员，点名时和「Codex」「Claude」一样算。
pub fn check_assignment_with(route: &Route, sources: &[Source], aliases: &[(String, String)]) -> Result<(), String> {
    if !crate::project_store::is_executor(&route.agent_id)
        || !matches!(
            route.purpose.as_str(),
            "solution" | "implementation" | "verification" | "closeout" | "reply"
        )
    {
        return Err("任务成员或用途无效".into());
    }
    if route.key.is_empty()
        || route.key.len() > 80
        || !route
            .key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("路由 key 无效".into());
    }
    let reference = Reference {
        source_id: route.source_id.clone(),
        quote: route.quote.clone(),
    };
    let user = source(sources, &reference, true)?;
    let text = clause(&user.text, &route.quote).to_lowercase();
    let named = [
        ("codex", "codex-win"),
        ("hermes", "hermes-win"),
        ("dsh", "dsh-win"),
        ("claude", "claude-win"),
    ]
    .into_iter()
    .filter(|(label, _)| text.contains(label))
    .map(|(_, id)| id)
    .chain(
        aliases
            .iter()
            .filter(|(name, _)| !name.is_empty() && text.contains(&name.to_lowercase()))
            .map(|(_, id)| id.as_str()),
    )
    .collect::<Vec<_>>();
    if route.subagent && route.purpose != "verification" {
        return Err("子代理只用于验证".into());
    }
    // 子代理验证由改动者派出，是否允许由 enqueue_routes 按条件判定，这里不按点名拦。
    // 点名按底层成员算：「让 Codex …」可以派给 Codex 的任一席位。
    let base = crate::project_store::base_agent(&route.agent_id);
    if !route.subagent && !named.is_empty() && !named.contains(&base) {
        return Err("这段用户原话已点名另一成员，管家不能换派".into());
    }
    // A task may refer to a predecessor, but must not also include standalone
    // instructions addressed only to other members from the same request.
    for sentence in route.quote.split(['。', '；', ';', '\n']) {
        let sentence = sentence.to_lowercase();
        let actors = [
            ("codex", "codex-win"),
            ("hermes", "hermes-win"),
            ("dsh", "dsh-win"),
            ("claude", "claude-win"),
        ]
        .into_iter()
        .filter(|(label, _)| sentence.contains(label))
        .map(|(_, id)| id)
        .chain(
            aliases
                .iter()
                .filter(|(name, _)| !name.is_empty() && sentence.contains(&name.to_lowercase()))
                .map(|(_, id)| id.as_str()),
        )
        .collect::<Vec<_>>();
        if !route.subagent && !actors.is_empty() && !actors.contains(&base) {
            return Err("任务引用夹带了其他成员的独立分工，请只引用当前成员对应的原文；前置结果通过context_ids传递".into());
        }
    }
    for id in &route.context_ids {
        if !sources.iter().any(|s| s.id == *id) {
            return Err("任务上下文引用不存在".into());
        }
    }
    Ok(())
}
fn normalized(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
fn model_named(model: &ModelOption, text: &str) -> bool {
    let value = normalized(text);
    let id = normalized(&model.id);
    (!id.is_empty() && value.contains(&id))
        || model.name.split(['·', '/']).any(|part| {
            let name = normalized(part);
            name.len() >= 4 && value.contains(&name)
        })
}
/// 给修改者建议的子代理模型：优先成员当前配置的低价档，其次同一提供商下的低价档（haiku/flash/mini/nano/lite），
/// 都没有就沿用成员配置；思考强度取 high（不支持时取该模型最高档）。
pub fn subagent_model(
    models: &[ModelOption],
    fallback: &RoleChoice,
) -> (Option<String>, Option<String>) {
    const CHEAP: [&str; 5] = ["haiku", "flash", "mini", "nano", "lite"];
    let tier = |model: &ModelOption| {
        let text = format!("{} {}", model.id, model.name).to_lowercase();
        CHEAP.iter().position(|word| text.contains(word))
    };
    let configured = fallback
        .model
        .as_ref()
        .and_then(|id| models.iter().find(|model| &model.id == id));
    let provider = configured.and_then(|model| model.provider_id.clone());
    let chosen = configured.filter(|model| tier(model).is_some()).or_else(|| {
        models
            .iter()
            .filter(|model| tier(model).is_some())
            .min_by_key(|model| {
                (
                    provider.is_some() && model.provider_id != provider,
                    model.efforts.is_empty(),
                    tier(model),
                )
            })
    });
    let Some(model) = chosen else {
        return (fallback.model.clone(), Some("high".into()));
    };
    let effort = if model.efforts.iter().any(|effort| effort == "high") {
        Some("high".into())
    } else {
        ["max", "xhigh"]
            .iter()
            .find(|level| model.efforts.iter().any(|effort| effort == *level))
            .map(|level| level.to_string())
    };
    (Some(model.id.clone()), effort)
}
fn provider_score(model: &ModelOption, text: &str) -> usize {
    let value = normalized(text);
    [model.provider_name.as_ref(), model.provider_id.as_ref()]
        .into_iter()
        .flatten()
        .map(|provider| normalized(provider))
        .filter(|name| !name.is_empty() && value.contains(name))
        .map(|name| name.len())
        .max()
        .unwrap_or(0)
}
fn model_family(model: &ModelOption) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(&model.id) {
        if let Some(name) = value
            .as_array()
            .and_then(|array| array.last())
            .and_then(Value::as_str)
        {
            return normalized(name);
        }
    }
    normalized(model.name.split('·').next_back().unwrap_or(&model.name))
}
fn named_effort(text: &str) -> Option<&'static str> {
    let text = text.to_lowercase();
    let mut choices = Vec::new();
    for name in [
        "xhigh", "high", "medium", "low", "minimal", "max", "ultra", "none", "off",
    ] {
        for (at, word) in text.match_indices(name) {
            let before = text[..at].chars().next_back();
            let after = text[at + word.len()..].trim_start().chars().next();
            if !before.is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '\\'))
                && !after.is_some_and(|c| {
                    c.is_ascii_alphanumeric() || matches!(c, '.' | '(' | '/' | '\\')
                })
            {
                choices.push((at, name));
            }
        }
    }
    for (label, value) in [
        ("高强度", "high"),
        ("思考高", "high"),
        ("低强度", "low"),
        ("思考低", "low"),
    ] {
        choices.extend(text.match_indices(label).map(|(at, _)| (at, value)));
    }
    choices
        .into_iter()
        .max_by_key(|(at, _)| *at)
        .map(|(_, name)| name)
}
pub fn overrides(
    route: &Route,
    sources: &[Source],
    models: &[ModelOption],
    fallback: &RoleChoice,
) -> Result<(Option<String>, Option<String>), String> {
    let reference = route.model_source.clone().unwrap_or(Reference {
        source_id: route.source_id.clone(),
        quote: route.quote.clone(),
    });
    let user = source(sources, &reference, true)?;
    let text = clause(&user.text, &reference.quote);
    let named = [
        ("codex", "codex-win"),
        ("hermes", "hermes-win"),
        ("dsh", "dsh-win"),
        ("claude", "claude-win"),
    ]
    .into_iter()
    .filter(|(label, _)| text.to_lowercase().contains(label))
    .map(|(_, id)| id)
    .collect::<Vec<_>>();
    // 只有用户原话里的点名才算授权；调度自己按任务选的模型/强度不在此列。
    let parameter_authority = route.model_source.is_some()
        || models.iter().any(|model| model_named(model, text))
        || (named_effort(text).is_some()
            && ["思考", "推理", "reasoning", "effort"]
                .iter()
                .any(|label| text.to_lowercase().contains(label)));
    if parameter_authority && !named.is_empty() && (named.len() != 1 || named[0] != route.agent_id)
    {
        return Err("模型与强度授权应对应当前成员，不能取用另一成员的参数".into());
    }
    if route.model_source.is_some()
        && named.is_empty()
        && !text.contains("所有")
        && !text.to_lowercase().contains("all ")
    {
        let scope = source(
            sources,
            &Reference {
                source_id: route.source_id.clone(),
                quote: route.quote.clone(),
            },
            true,
        )?;
        if !clause(&scope.text, &route.quote).contains(&reference.quote) {
            return Err("跨句模型授权需明确对应当前成员".into());
        }
    }
    let matches = models
        .iter()
        .filter(|model| model_named(model, text))
        .collect::<Vec<_>>();
    let provider_score_max = matches
        .iter()
        .map(|model| provider_score(model, text))
        .max()
        .unwrap_or(0);
    let chosen = if let Some(id) = &route.model {
        let model = models
            .iter()
            .find(|model| model.id == *id)
            .ok_or("这个模型不在该成员的模型目录里")?;
        // 用户点名了模型就以用户为准；没点名时调度可以按任务自选。
        if !matches.is_empty() && !model_named(model, text) {
            return Err("用户原话点名了模型，以用户为准".into());
        }
        let alternative_provider = matches.iter().any(|other| {
            other.provider_id.is_some()
                && other.provider_id != model.provider_id
                && model_family(other) == model_family(model)
        });
        if alternative_provider
            && ((provider_score_max > 0 && provider_score(model, text) < provider_score_max)
                || (provider_score_max == 0 && fallback.model.as_ref() != Some(id)))
        {
            return Err("用户没有点名另一条提供商线路，应沿用项目配置或问清".into());
        }
        Some(model)
    } else if matches.len() == 1 {
        matches.first().copied()
    } else {
        let named_providers = matches
            .iter()
            .filter(|model| {
                provider_score_max > 0 && provider_score(model, text) == provider_score_max
            })
            .copied()
            .collect::<Vec<_>>();
        if named_providers.len() == 1 {
            named_providers.first().copied()
        } else if matches
            .windows(2)
            .all(|pair| model_family(pair[0]) == model_family(pair[1]))
        {
            matches
                .iter()
                .find(|model| Some(&model.id) == fallback.model.as_ref())
                .copied()
        } else {
            return Err("原文提到多个型号，请通过model字段明确用户要求的目标型号，不能猜选".into());
        }
    };
    if matches.len() > 1 && chosen.is_none() {
        return Err("该型号有多条提供商线路，需要保留当前线路或向用户问清".into());
    }
    let effort_context = !matches.is_empty()
        || ["思考", "推理", "reasoning", "effort"]
            .iter()
            .any(|label| text.to_lowercase().contains(label));
    let named = effort_context
        .then(|| named_effort(text))
        .flatten()
        .map(str::to_owned);
    // 用户点名了强度就以用户为准；没点名时用调度按任务选的（可为空＝沿用项目配置）。
    let effort = match (named, &route.effort) {
        (Some(user), Some(chosen)) if *chosen != user => {
            return Err("用户原话点名了思考强度，以用户为准".into())
        }
        (Some(user), _) => Some(user),
        (None, chosen) => chosen.clone(),
    };
    let model = chosen.map(|model| model.id.clone());
    let effective = model.as_deref().or(fallback.model.as_deref());
    crate::models::validate_selection(
        models,
        effective,
        effort.as_deref().or(fallback.effort.as_deref()),
        fallback.model.as_deref(),
    )?;
    Ok((model, effort))
}
/// 成员失败是否因为模型额度用完或被限流：这类失败等额度恢复后重试即可，不该让整轮协作中断。
pub fn quota_exhausted(error: &str) -> bool {
    let text = error.to_lowercase();
    [
        "限流",
        "额度",
        "rate limit",
        "rate_limit",
        "ratelimit",
        "usage limit",
        "quota",
        "too many requests",
        "429",
    ]
    .iter()
    .any(|word| text.contains(word))
}
/// 额度等待的重试间隔（分钟）：5、10、15……封顶 30。
pub fn quota_retry_minutes(waits: u32) -> u64 {
    (5 * (u64::from(waits) + 1)).min(30)
}
/// 子代理验证的任务说明：由修改者本人用自带的子代理工具派低价模型、高思考强度的子代理独立验证。
pub fn subagent_scope(agent: &str, cheap: Option<&str>) -> String {
    let tool = match agent {
        "claude-win" => format!(
            "用 Agent 工具派子代理，model 设为 {}、effort 设为 high",
            cheap.unwrap_or("haiku")
        ),
        _ => format!(
            "用你自带的子代理/委派工具派子代理，选低价模型{}、高思考强度",
            cheap.map(|model| format!("（如 {model}）")).unwrap_or_default()
        ),
    };
    format!(
        "本次是子代理验证：你是这些改动的作者，不要自己验证，{tool}。子代理的任务说明里写清用户原话、被验证的改动与验证范围，要求它独立验证、不修改被验证的源文件（需要时可写验证脚本），并在最后写“验证结论：通过”或“验证结论：未通过”及问题清单。assigned_excerpt 若点名了其他验证成员，本次由你的子代理代为验证。拿到结果后如实转述，不改写结论；你的最后一行照抄子代理的验证结论。若你没有子代理工具，说明原因并写“验证结论：未通过”，不要自己代验。"
    )
}
/// 成员交代这项超出能力、要换强模型接手：交付里单独一行「需要升档：原因」，返回原因。
pub fn escalation(output: &str) -> Option<&str> {
    output.lines().rev().find_map(|line| {
        let line = line.trim().trim_start_matches(['*', '-', ' ']);
        line.strip_prefix("需要升档：").or_else(|| line.strip_prefix("需要升档:")).map(str::trim)
    })
}

/// 成员交付摘要的开头；调度只读这一段。
pub const DELIVERY_MARK: &str = "交付摘要：";

/// 调度看到的成员结果：有交付摘要就只给摘要，没有就留头尾各一段，省得每轮重发全文。
pub fn delivery_summary(output: &str) -> String {
    if let Some(at) = output.rfind(DELIVERY_MARK) {
        return output[at..].chars().take(2400).collect();
    }
    let chars = output.chars().collect::<Vec<_>>();
    if chars.len() <= 3000 {
        return output.to_owned();
    }
    format!(
        "{}\n……（中间省略 {} 字）……\n{}",
        chars[..1200].iter().collect::<String>(),
        chars.len() - 2400,
        chars[chars.len() - 1200..].iter().collect::<String>()
    )
}

pub fn member_prompt(
    agent: &str,
    purpose: &str,
    user_request: &str,
    quote: &str,
    contexts: Vec<Value>,
    brief: Option<&str>,
    allow_subagents: bool,
) -> Value {
    let scope = match purpose {
        "solution" => "本次只完成用户要求的方案或分析，不提前执行引用中的后续实现、验证或收尾。",
        "implementation" => "本次完成被分配的实现范围，不接取引用中分给其他成员的工作。若 context_sources 里有验证成员报告的问题，本次就是返修：逐条修复这些问题，并说明每条如何处理。",
        "verification" => "本次完成被分配的验证范围，按用户原话决定是否需要编写验证脚本。最后单独一行写“验证结论：通过”或“验证结论：未通过”；未通过时列出问题清单（文件、现象、复现方式），供管家安排返修。",
        "closeout" => "本次完成用户要求的整理或收尾，报告实际完成的内容。",
        _ => "本次回复当前问题，不提前执行引用中的其他工作。",
    };
    let mut prompt = serde_json::json!({
      "instruction":member_instruction(),
      "assigned_agent":agent,"purpose":purpose,"task_scope":scope,"assigned_excerpt":quote,"context_sources":contexts
    });
    // 调度写了任务说明时不再附用户整段原话：那是说给调度的（「做 2、3、4」这类），成员拿到会自己去猜，和说明打架。
    match brief.map(str::trim).filter(|brief| !brief.is_empty()) {
        Some(brief) => prompt["coordinator_brief"] = Value::from(brief),
        None => prompt["user_request"] = Value::from(user_request),
    }
    if allow_subagents {
        prompt["subagents"] = Value::from("本任务允许用你自带的子 agent 工具拆分并行调研或做独立验证；子 agent 尽量选低价模型，把子 agent 的结论整理进你的回复。");
    }
    prompt
}
