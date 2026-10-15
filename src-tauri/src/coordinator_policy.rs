//! A coordinator chooses routes. Member instructions always come from existing sources.
use crate::{models::ModelOption, project_store::RoleChoice};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MANAGER_ACTIVITY: &str = "coordinate";
/// 管家所在成员处理原仓库合并/变基中途状态的回合。
pub const GIT_REPAIR_ACTIVITY: &str = "resolve:git";
pub const MANAGER_INSTRUCTION: &str = "你是本项目的调度管家，仅负责理解用户原话里的分工、安排顺序、接收成员结果并继续协调。你不出技术方案、不写代码、不验证项目、不整理文件，也不能向成员增加业务指示。方案、实现、验证、文档和收尾都可按用户原话交给任意技术成员。成员任务只能引用 sources 中真实的用户原话或成员结果；routes 没有 instructions 字段，严禁自行添加。用户明确的成员、模型、范围、先后顺序和收尾职责优先；没有明确模型时留空，沿用该成员的项目配置。source_id 引用用户消息，quote 为其中完整的一段原文（不得改字或抽掉同句中的成员/模型要求）；context_ids 可以引用已完成方案和相关成员结果，不能引用你的内部回合。仅当用户要求方案时先派方案任务，收到结果后按原要求派后续工作；无需方案的目标可直接分派；不要把一段需求压缩成固定三阶段或把全部实现强行交给一个成员。purpose 按用户原话的性质选择：solution 为方案或分析；implementation 仅用于需要修改项目文件的工作；verification 为验证；closeout 为整理收尾；只需回答问题、说明情况或查询事实且不改文件时用 reply。尊重暂停和纠正，保留已经完成的结果；依赖未完成的工作等待，独立工作可继续。成员只能按指定任务工作，发现分工冲突要向用户澄清。需要用户信息时 action=ask_user，结束并等待真实回答。用户纠正方向时，先处理纠正，再启动未开始的任务；仅可用 withdraw 引用新的用户原话取消未完成旧任务，已完成改动不得撤销；正在运行的任务先等待或由用户中断。恢复原任务应保留原 source_id/quote，新的模型授权放在 model_source、纠正内容放在 context_ids。验证结果写明“验证结论：未通过”时，用原实现任务的同一 source_id/quote 再派给原实现成员返修，context_ids 引用该验证结果；返修完成后用原验证任务的同一原文再派给验证成员复核，context_ids 引用返修结果；同一项工作最多返修两轮，仍未通过就 ask_user 询问用户；用户回答同意继续后可再返修两轮，context_ids 同时引用该回答。指定的验证成员已有两次未通过或异常（失败）时，或用户要求用执行者/改动者的子代理验证时，可派子代理验证：purpose 为 verification、subagent 为 true、agent_id 为最近完成实现的成员，source_id/quote 仍引用用户的验证原文，context_ids 引用最新实现结果；该成员会用自带的子代理工具派低价模型、高思考强度的子代理去验证，管家不直接派子代理，model/effort 留空。验证通过后再继续后续分工。任务失败不能声称完成；可以 resume 原任务保留工作树，不能擅自换掉用户明确指定的成员/模型。仅当任务都完成且目标有成员结果支持时 finish，用 result 引用成员的原文回复；wait 只用于真实失败或被中断任务等待恢复；没有此类任务时不要 wait，应按用户原文继续分派后续工作，或在目标完成后 finish。不要把已完成的成员回合当作仍在运行，也不要让用户额外点击继续来启动尚未分派的工作。你的普通说明只用于内部路由，不会传给成员。只返回 JSON：{action:'delegate'|'ask_user'|'wait'|'finish',routes?:[{key:string,task_id?:string,purpose:'solution'|'implementation'|'verification'|'closeout'|'reply',agent_id:string,source_id:string,quote:string,context_ids?:string[],depends_on?:string[],model?:string|null,effort?:string|null,model_source?:{source_id:string,quote:string},subagent?:boolean}],withdraw?:[{task_id:string,source_id:string,quote:string}],question?:{prompt:string,options:string[]},result?:{source_id:string,quote:string}}。依赖可以用本回合之前声明的 route.key 或已有 task_id。不得输出补丁、凭据、工具调用或额外字段。";

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
    #[serde(default)]
    pub question: Option<Question>,
    #[serde(default)]
    pub result: Option<Reference>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Source {
    pub id: String,
    pub kind: String,
    pub text: String,
}

pub fn parse(raw: &str) -> Result<Decision, String> {
    let document = crate::project_store::json_document(raw, 64_000)?;
    let value: Decision = serde_json::from_str(document)
        .map_err(|_| "管家路由格式无效；不能给成员新增自由指示".to_owned())?;
    if !matches!(
        value.action.as_str(),
        "delegate" | "ask_user" | "wait" | "finish"
    ) || value.routes.len() > 32
    {
        return Err("管家需返回可执行的路由动作".into());
    }
    if value.withdraw.len() > 32 {
        return Err("每回合撤回任务过多".into());
    }
    if value.action == "delegate" && value.routes.is_empty() {
        return Err("分派动作没有任务".into());
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
pub fn check_assignment(route: &Route, sources: &[Source]) -> Result<(), String> {
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
    let parameter_authority = route.model.is_some()
        || route.effort.is_some()
        || route.model_source.is_some()
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
            .ok_or("用户指定的模型不在此成员目录里")?;
        if !model_named(model, text) {
            return Err("管家不能新增用户未指定的模型；留空使用项目配置".into());
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
    let effort_context = chosen.is_some()
        || ["思考", "推理", "reasoning", "effort"]
            .iter()
            .any(|label| text.to_lowercase().contains(label));
    let effort = effort_context
        .then(|| named_effort(text))
        .flatten()
        .map(str::to_owned);
    if route
        .effort
        .as_ref()
        .is_some_and(|value| Some(value) != effort.as_ref())
    {
        return Err("管家不能新增用户未指定的思考强度".into());
    }
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
pub fn member_prompt(
    agent: &str,
    purpose: &str,
    user_request: &str,
    quote: &str,
    contexts: Vec<Value>,
) -> Value {
    let scope = match purpose {
        "solution" => "本次只完成用户要求的方案或分析，不提前执行引用中的后续实现、验证或收尾。",
        "implementation" => "本次完成被分配的实现范围，不接取引用中分给其他成员的工作。若 context_sources 里有验证成员报告的问题，本次就是返修：逐条修复这些问题，并说明每条如何处理。",
        "verification" => "本次完成被分配的验证范围，按用户原话决定是否需要编写验证脚本。最后单独一行写“验证结论：通过”或“验证结论：未通过”；未通过时列出问题清单（文件、现象、复现方式），供管家安排返修。",
        "closeout" => "本次完成用户要求的整理或收尾，报告实际完成的内容。",
        _ => "本次回复当前问题，不提前执行引用中的其他工作。",
    };
    serde_json::json!({
      "instruction":"按用户原话与当前任务原文完成工作。管家只选择了路由，没有给你追加业务指示。user_request 是目标来源，assigned_excerpt 是本次分工范围，context_sources 是已有方案或成员结果；不要执行其他成员的职责。若路由成员、任务用途或方案内容与用户明确要求冲突，向用户提问，不擅自改范围或换人。需要提交、推送或发布时，以当前用户明确要求为授权，不能把成员建议当作授权。保留已完成改动，报告实际结果与未解决的问题。可用原生工具工作、向成员沟通，问题正常发到团队等待真实回答。",
      "assigned_agent":agent,"purpose":purpose,"task_scope":scope,"user_request":user_request,"assigned_excerpt":quote,"context_sources":contexts
    })
}
