//! 能力与成本画像：按「成员 + 模型（含线路）+ 思考强度 + 任务类别」汇总历史执行，
//! 给调度看成功率、返修与完整任务消耗。只从已有的执行记录与任务路由算，不另建表。
use serde_json::{json, Value};
use std::collections::HashMap;

/// 调度可选的任务类别（由调度按任务判断，不做关键词匹配）。
pub const CATEGORIES: [&str; 9] = [
    "architecture",
    "feature",
    "bugfix",
    "algorithm",
    "refactor",
    "testing",
    "docs",
    "ops",
    "integration",
];
pub const COMPLEXITIES: [&str; 3] = ["low", "medium", "high"];

/// 一次执行（attempt）连同它所属任务的路由信息。
#[derive(Clone, Debug)]
pub struct Sample {
    pub workflow_id: String,
    pub task_id: String,
    pub agent_id: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub output: String,
    pub usage: Option<crate::project_store::Usage>,
    pub created_at: i64,
    pub purpose: String,
    pub category: Option<String>,
    pub complexity: Option<String>,
    pub source_id: String,
    pub quote: String,
    pub context_ids: Vec<String>,
}

/// 撞限额、被用户停下或中断、环境/连接问题导致的失败不算能力问题，不计入成功率。
pub fn environmental(status: &str, error: Option<&str>) -> bool {
    if status == "interrupted" {
        return true;
    }
    let error = error.unwrap_or_default().to_lowercase();
    [
        "限额", "限流", "额度", "稍后", "不可用", "-32603", "rate limit", "unavailable", "网络", "network", "429", "usage limit", "已停止", "暂停", "中断", "超时", "超过当前工作流剩余时间",
        "认证", "登录", "连接", "拒绝请求", "未运行", "未安装", "overloaded", "timeout", "auth",
    ]
    .iter()
    .any(|word| error.contains(word))
}

/// 验证结论：Some(true) 通过、Some(false) 未通过、None 没写结论。
pub fn verdict(output: &str) -> Option<bool> {
    let output = output.replace([' ', '　', '*'], "").replace(':', "：");
    if output.contains("验证结论：未通过") {
        Some(false)
    } else if output.contains("验证结论：通过") {
        Some(true)
    } else {
        None
    }
}

/// 把同一协作里的任务按显式结果引用归组（与 logical_tasks 同一规则）：
/// 引用了另一项任务的结果，且同一段原文或其中一方是验证。返回 task_id → 组根（最早出现的任务）。
pub fn link_tasks<'a>(
    items: impl Iterator<Item = (&'a str, &'a str, &'a str, &'a str, &'a [String])> + Clone,
) -> HashMap<String, String> {
    let order: Vec<&str> = {
        let mut seen = Vec::new();
        for (task, ..) in items.clone() {
            if !seen.contains(&task) {
                seen.push(task);
            }
        }
        seen
    };
    let info: HashMap<&str, (&str, &str, &str, &[String])> = items
        .map(|(task, purpose, source, quote, context)| (task, (purpose, source, quote, context)))
        .collect();
    let mut parent: HashMap<String, String> = order.iter().map(|id| (id.to_string(), id.to_string())).collect();
    fn root(parent: &HashMap<String, String>, id: &str) -> String {
        let mut id = id.to_owned();
        while parent[&id] != id {
            id = parent[&id].clone();
        }
        id
    }
    let rank = |id: &str| order.iter().position(|item| *item == id).unwrap_or(usize::MAX);
    for task in &order {
        let (purpose, source, quote, context) = info[task];
        for other in context {
            let Some(&(that_purpose, that_source, that_quote, _)) = info.get(other.as_str()) else {
                continue;
            };
            if other == task || purpose == "reply" || that_purpose == "reply" {
                continue;
            }
            let linked = (source == that_source && quote == that_quote)
                || purpose == "verification"
                || that_purpose == "verification";
            if !linked {
                continue;
            }
            let (a, b) = (root(&parent, task), root(&parent, other));
            if a != b {
                let (keep, merge) = if rank(&a) <= rank(&b) { (a, b) } else { (b, a) };
                parent.insert(merge, keep);
            }
        }
    }
    order.iter().map(|id| (id.to_string(), root(&parent, id))).collect()
}

#[derive(Default)]
struct Tally {
    samples: u32,
    weight: f64,
    passes: f64,
    tokens: (f64, f64),
    opened: f64,
    first_pass: f64,
    task_tokens: (f64, f64),
    repairs: (f64, f64),
    complexity: std::collections::BTreeMap<String, u32>,
}

/// 先验成功率与其权重：样本少时向它收缩，避免两次成功就当成 100%。
const PRIOR_RATE: f64 = 0.7;
const PRIOR_WEIGHT: f64 = 2.0;
/// 旧样本按 90 天半衰期衰减：模型、线路或成员版本更新后，早先的表现影响逐渐变小。
const HALF_LIFE_MS: f64 = 90.0 * 24.0 * 3600.0 * 1000.0;

fn smooth(passes: f64, weight: f64) -> f64 {
    ((passes + PRIOR_RATE * PRIOR_WEIGHT) / (weight + PRIOR_WEIGHT) * 100.0).round() / 100.0
}

/// 汇总画像。返回按样本数排序的组合列表（最多 limit 条），字段为空表示没有可用数据。
pub fn profiles(samples: &[Sample], now: i64, limit: usize) -> Vec<Value> {
    let mut by_workflow: HashMap<&str, Vec<&Sample>> = HashMap::new();
    for sample in samples {
        by_workflow.entry(&sample.workflow_id).or_default().push(sample);
    }
    let mut tallies: HashMap<(String, String, String, String), Tally> = HashMap::new();
    for attempts in by_workflow.values() {
        let groups = link_tasks(attempts.iter().map(|sample| {
            (
                sample.task_id.as_str(),
                sample.purpose.as_str(),
                sample.source_id.as_str(),
                sample.quote.as_str(),
                sample.context_ids.as_slice(),
            )
        }));
        // 每项验证任务按时间顺序的结论；被验证的任务在验证开始前完成的那次执行，按这个结论判。
        let verifications: Vec<(&Sample, Option<bool>)> = attempts
            .iter()
            .filter(|sample| sample.purpose == "verification" && sample.status == "completed")
            .map(|sample| (*sample, verdict(&sample.output)))
            .collect();
        // 组内消耗合计（未知就整组未知）与返修次数，记在组里第一次执行的组合上。
        let mut group_tokens: HashMap<&str, Option<u64>> = HashMap::new();
        let mut group_repairs: HashMap<&str, u32> = HashMap::new();
        let mut group_failed: HashMap<&str, bool> = HashMap::new();
        let mut rework_seen = std::collections::HashSet::new();
        for sample in attempts {
            let group = groups[&sample.task_id].as_str();
            let spent = sample.usage.as_ref().and_then(|usage| usage.spent());
            let entry = group_tokens.entry(group).or_insert(Some(0));
            *entry = entry.zip(spent).map(|(a, b)| a + b);
            // 返修：引用了验证结论的再实现，或同一任务因非环境原因失败后的重跑。
            // 并行的几项实现、撞限流或被停下后的重试都不算；同一项返修任务的多次执行只算一次。
            let after_review = sample.context_ids.iter().any(|context| {
                attempts.iter().any(|other| &other.task_id == context && other.purpose == "verification")
            });
            let rerun = attempts.iter().any(|other| {
                other.task_id == sample.task_id
                    && other.created_at < sample.created_at
                    && other.status == "failed"
                    && !environmental(&other.status, other.error.as_deref())
            });
            if sample.purpose != "verification" && (after_review || rerun) && rework_seen.insert(sample.task_id.as_str()) {
                *group_repairs.entry(group).or_default() += 1;
            }
            let failed = (sample.status == "failed" && !environmental(&sample.status, sample.error.as_deref()))
                || (sample.purpose == "verification" && verdict(&sample.output) == Some(false));
            *group_failed.entry(group).or_default() |= failed;
        }
        let mut opened = std::collections::HashSet::new();
        for sample in attempts {
            if matches!(sample.purpose.as_str(), "verification" | "reply") {
                continue;
            }
            if sample.status != "completed" && environmental(&sample.status, sample.error.as_deref()) {
                continue;
            }
            let passed = sample.status == "completed"
                && verifications
                    .iter()
                    .filter(|(check, _)| {
                        check.created_at > sample.created_at && check.context_ids.contains(&sample.task_id)
                    })
                    .min_by_key(|(check, _)| check.created_at)
                    .is_none_or(|(_, verdict)| *verdict != Some(false));
            let weight = 0.5f64.powf((now - sample.created_at).max(0) as f64 / HALF_LIFE_MS);
            let group = groups[&sample.task_id].as_str();
            let first = opened.insert(group);
            let category = sample.category.clone().unwrap_or_else(|| "uncategorized".into());
            let key = (
                crate::project_store::base_agent(&sample.agent_id).to_owned(),
                sample.model.clone().unwrap_or_else(|| "default".into()),
                sample.effort.clone().unwrap_or_else(|| "default".into()),
                category,
            );
            let tally = tallies.entry(key).or_default();
            tally.samples += 1;
            if let Some(complexity) = &sample.complexity {
                *tally.complexity.entry(complexity.clone()).or_default() += 1;
            }
            tally.weight += weight;
            tally.passes += if passed { weight } else { 0.0 };
            if let Some(spent) = sample.usage.as_ref().and_then(|usage| usage.spent()) {
                tally.tokens.0 += spent as f64 * weight;
                tally.tokens.1 += weight;
            }
            if first {
                tally.opened += weight;
                tally.first_pass += if !group_failed[group] { weight } else { 0.0 };
                tally.repairs.0 += group_repairs.get(group).copied().unwrap_or(0) as f64 * weight;
                tally.repairs.1 += weight;
                if let Some(total) = group_tokens[group] {
                    tally.task_tokens.0 += total as f64 * weight;
                    tally.task_tokens.1 += weight;
                }
            }
        }
    }
    let mut rows: Vec<_> = tallies.into_iter().collect();
    rows.sort_by(|a, b| b.1.samples.cmp(&a.1.samples).then_with(|| a.0.cmp(&b.0)));
    rows.into_iter()
        .take(limit)
        .map(|((agent, model, effort, category), tally)| {
            let average = |(sum, weight): (f64, f64)| (weight > 0.0).then(|| (sum / weight).round() as u64);
            json!({
                "agent": agent, "model": model, "effort": effort, "category": category,
                "samples": tally.samples,
                "success_rate": smooth(tally.passes, tally.weight),
                "first_pass_rate": (tally.opened > 0.0).then(|| smooth(tally.first_pass, tally.opened)),
                "avg_attempt_tokens": average(tally.tokens),
                "avg_task_tokens": average(tally.task_tokens),
                "avg_repairs": (tally.repairs.1 > 0.0).then(|| (tally.repairs.0 / tally.repairs.1 * 10.0).round() / 10.0),
                "complexity": (!tally.complexity.is_empty()).then_some(&tally.complexity),
                "confidence": match tally.samples { 0..=4 => "low", 5..=14 => "medium", _ => "high" },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_store::Usage;

    fn sample(task: &str, agent: &str, model: &str, status: &str, purpose: &str, at: i64) -> Sample {
        Sample {
            workflow_id: "w".into(), task_id: task.into(), agent_id: agent.into(), model: Some(model.into()),
            effort: Some("high".into()), status: status.into(), error: None, output: String::new(),
            usage: Some(Usage { input: 100, ..Default::default() }), created_at: at, purpose: purpose.into(),
            category: Some("bugfix".into()), complexity: None, source_id: "u".into(), quote: "修登录".into(), context_ids: vec![],
        }
    }

    #[test]
    fn rate_limits_and_user_stops_do_not_count_against_the_model() {
        let mut limited = sample("t1", "dsh-win", "flash", "failed", "implementation", 1);
        limited.error = Some("原生 agent session/prompt 请求失败：模型服务限流，请稍后继续当前任务（协议代码 -32603）".into());
        let stopped = sample("t2", "dsh-win", "flash", "interrupted", "implementation", 2);
        let done = sample("t3", "dsh-win", "flash", "completed", "implementation", 3);
        let rows = profiles(&[limited, stopped, done], 3, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["samples"], 1);
        // 一次成功、先验 0.7×2：(1+1.4)/(1+2)=0.8，小样本不给 100%。
        assert_eq!(rows[0]["success_rate"], 0.8);
        assert_eq!(rows[0]["confidence"], "low");
    }

    #[test]
    fn failed_verification_marks_the_verified_attempt_and_rework_lands_on_the_opener() {
        let first = sample("impl", "dsh-win", "flash", "completed", "implementation", 1);
        let mut check = sample("verify", "claude-win", "haiku", "completed", "verification", 2);
        check.output = "交付摘要：登录仍报错\n验证结论：未通过".into();
        check.context_ids = vec!["impl".into()];
        let mut repair = sample("repair", "codex-win", "gpt", "completed", "implementation", 3);
        repair.context_ids = vec!["verify".into()];
        let mut recheck = sample("recheck", "claude-win", "haiku", "completed", "verification", 4);
        recheck.output = "验证结论：通过".into();
        recheck.context_ids = vec!["repair".into()];
        let rows = profiles(&[first, check, repair, recheck], 4, 10);
        let row = |agent: &str| rows.iter().find(|row| row["agent"] == agent).unwrap();
        let dsh = row("dsh-win");
        // DSH 那次被验证否决：成功率 (0+1.4)/3≈0.47；它开的头，整组 4 次执行 400 token、返修 1 次都记在它身上。
        assert_eq!((dsh["success_rate"].as_f64(), dsh["first_pass_rate"].as_f64()), (Some(0.47), Some(0.47)));
        assert_eq!((dsh["avg_task_tokens"].as_u64(), dsh["avg_repairs"].as_f64()), (Some(400), Some(1.0)));
        let codex = row("codex-win");
        assert_eq!((codex["success_rate"].as_f64(), codex["first_pass_rate"].as_f64()), (Some(0.8), None));
        // 验证执行本身不进成功率画像。
        assert!(rows.iter().all(|row| row["agent"] != "claude-win"));
    }

    #[test]
    fn model_effort_and_harness_are_profiled_separately_and_old_samples_fade() {
        let day = 24 * 3600 * 1000;
        let mut low = sample("a", "dsh-win", "flash", "completed", "implementation", 0);
        low.effort = Some("low".into());
        let high = sample("b", "dsh-win", "flash", "completed", "implementation", 0);
        let hermes = sample("c", "hermes-win", "flash", "completed", "implementation", 0);
        assert_eq!(profiles(&[low, high, hermes], 0, 10).len(), 3);
        let old_fail = sample("d", "codex-win", "gpt", "failed", "implementation", 0);
        let new_pass = sample("e", "codex-win", "gpt", "completed", "implementation", 360 * day);
        let rows = profiles(&[old_fail, new_pass], 360 * day, 10);
        // 一年前的失败只剩 1/16 权重：(1+1.4)/(1.0625+2)≈0.78，高于不衰减时的 (1+1.4)/4=0.6。
        assert_eq!(rows[0]["success_rate"], 0.78);
    }

    #[test]
    fn verdict_reads_both_colons_and_markdown_bold() {
        assert_eq!(verdict("**验证结论：未通过**"), Some(false));
        assert_eq!(verdict("验证结论: 通过"), Some(true));
        assert_eq!(verdict("没写结论"), None);
    }
}
