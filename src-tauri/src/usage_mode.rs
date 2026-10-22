//! 成员报的用量是「本回合」还是「整条会话累计」，各家 CLI 不一样，同一家换个版本、换条网关也可能变。
//! 不逐家写死：执行结束时保留原样报的数（reported_usage），从同一条会话前后两次的报数里自己判断口径。
//!
//! 判断依据：累计值不会变小，而且续接的回合一开头就要把上一回合结束时的整段上下文再读一遍，
//! 所以累计口径下，后一次的输入至少比前一次多出上一回合的上下文。按回合报的数大小随任务起伏，
//! 很快就会出现不满足的一对。每次有执行结束就按这位成员全部近期报数重判、重算，
//! 口径变了（升级 CLI、换网关）也会自己跟上，不用谁来通知换口径。

use crate::project_store::Usage;
use rusqlite::{params, Connection};

/// 连着这么多对都像累计，才认定那一段报的是累计。
const RUN: usize = 5;
/// 重算的范围：最近两周结束的执行。
const WINDOW_MS: i64 = 14 * 24 * 3_600_000;

/// 后一次报数是否可能是前一次的累计延续。
fn continues(previous: &Usage, current: &Usage) -> bool {
    previous.total() > 0
        && current.input >= previous.input + previous.context * 9 / 10
        && current.output >= previous.output
        && current.cached >= previous.cached
        && current.reasoning >= previous.reasoning
}

fn minus(current: &Usage, previous: &Usage) -> Usage {
    Usage {
        input: current.input.saturating_sub(previous.input),
        cached: current.cached.saturating_sub(previous.cached),
        cache_write: current.cache_write.saturating_sub(previous.cache_write),
        output: current.output.saturating_sub(previous.output),
        reasoning: current.reasoning.saturating_sub(previous.reasoning),
        cost_usd: match (current.cost_usd, previous.cost_usd) {
            (Some(now), Some(before)) => Some(((now - before).max(0.0) * 1e6).round() / 1e6),
            (now, _) => now,
        },
        ..current.clone()
    }
}

/// 一位成员的各条会话（每条按时间排好的原始报数），算出每次执行该记的用量。
/// 把这位成员所有「前后两次报数」按时间排开，连续 RUN 对以上都像累计的那一段才按差额记：
/// 累计口径的报数一段到底都满足，按回合报的很少能连着这么多对都满足；
/// 中途换了口径（升级 CLI、换网关）也只影响换之前那一段。
pub fn effective(sessions: &[Vec<(i64, Usage)>]) -> Vec<Vec<Usage>> {
    let mut pairs: Vec<(i64, usize, usize, bool)> = sessions
        .iter()
        .enumerate()
        .flat_map(|(session, reports)| {
            reports
                .windows(2)
                .enumerate()
                .map(move |(index, pair)| (pair[1].0, session, index + 1, continues(&pair[0].1, &pair[1].1)))
        })
        .collect();
    pairs.sort_by_key(|pair| pair.0);
    let mut result: Vec<Vec<Usage>> = sessions.iter().map(|reports| reports.iter().map(|r| r.1.clone()).collect()).collect();
    let mut start = 0;
    while start < pairs.len() {
        let end = (start..pairs.len()).find(|&index| !pairs[index].3).unwrap_or(pairs.len());
        if end - start >= RUN {
            for &(_, session, index, _) in &pairs[start..end] {
                result[session][index] = minus(&sessions[session][index].1, &sessions[session][index - 1].1);
            }
        }
        start = end + 1;
    }
    result
}

/// 某位成员一次执行结束后调用：按它最近的报数重新判断口径，把有出入的记录改过来。
pub fn settle(connection: &Connection, agent_id: &str, now_ms: i64) -> rusqlite::Result<()> {
    let base = crate::project_store::base_agent(agent_id);
    let mut query = connection.prepare(
        "SELECT id,native_thread_id,created_at,COALESCE(reported_usage,usage),usage FROM project_attempts
         WHERE (agent_id=?1 OR agent_id LIKE ?1 || '-%') AND created_at>=?2 AND COALESCE(native_thread_id,'')<>''
           AND COALESCE(reported_usage,usage) IS NOT NULL AND status IN ('completed','failed','interrupted')
         ORDER BY native_thread_id,created_at,rowid",
    )?;
    let rows: Vec<(String, String, i64, String, String)> = query
        .query_map(params![base, now_ms - WINDOW_MS], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut sessions: Vec<Vec<(i64, Usage)>> = Vec::new();
    let mut records: Vec<Vec<(&str, &str, &str)>> = Vec::new();
    let mut last_session = None;
    for (id, session, created, raw, stored) in &rows {
        let Ok(usage) = serde_json::from_str::<Usage>(raw) else { continue };
        // 只知道上下文占用的不是消耗，不参与判断，也不改。
        if usage.context_only {
            continue;
        }
        if last_session != Some(session) {
            sessions.push(Vec::new());
            records.push(Vec::new());
            last_session = Some(session);
        }
        sessions.last_mut().unwrap().push((*created, usage));
        records.last_mut().unwrap().push((id, raw, stored));
    }
    for (usages, records) in effective(&sessions).iter().zip(&records) {
        for (usage, (id, raw, stored)) in usages.iter().zip(records) {
            // 比结构不比文本：旧记录少几个后来才加的字段，内容一样就不改。
            if serde_json::from_str::<Usage>(stored).ok().as_ref() == Some(usage) {
                continue;
            }
            if let Ok(text) = serde_json::to_string(usage) {
                connection.execute(
                    "UPDATE project_attempts SET usage=?1,reported_usage=COALESCE(reported_usage,?2) WHERE id=?3",
                    params![text, raw, id],
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(input: u64, output: u64, context: u64) -> Usage {
        Usage { input, output, context, ..Default::default() }
    }

    /// 累计口径：每次续接都把上一回合的上下文再读一遍，累计值只增不减。
    fn totals(from: i64, count: usize) -> Vec<(i64, Usage)> {
        (0..count as u64).map(|n| (from + n as i64, used(1_000 + n * 1_000, 100 + n * 50, 300 + n * 10))).collect()
    }

    #[test]
    fn running_totals_are_recognised_and_split_per_turn() {
        let result = effective(&[totals(1, 6)]);
        let turns: Vec<u64> = result[0].iter().map(Usage::total).collect();
        assert_eq!(turns, [1_100, 1_050, 1_050, 1_050, 1_050, 1_050]);
        assert_eq!(result[0][5].context, 350);
    }

    #[test]
    fn per_turn_reports_stay_as_reported() {
        // 按回合报：大小随任务起伏，连不成一长段。
        let per_turn: Vec<(i64, Usage)> = [1_000, 2_000, 900, 5_000, 6_500, 800, 7_000]
            .iter()
            .enumerate()
            .map(|(n, &input)| (n as i64, used(input, input / 10, 300)))
            .collect();
        let result = effective(&[per_turn.clone()]);
        assert_eq!(result[0], per_turn.iter().map(|r| r.1.clone()).collect::<Vec<_>>());
    }

    #[test]
    fn a_short_growing_streak_is_not_enough_evidence() {
        // 只有几对看着像累计，可能只是后面的任务更大：证据不够，保持原样。
        assert_eq!(effective(&[totals(1, 4)])[0][3].total(), 4_250);
    }

    #[test]
    fn switching_from_totals_to_per_turn_only_affects_the_earlier_stretch() {
        // 前半段报累计，之后（升级了 CLI）改报本回合：前半段按差额，后半段原样。
        let mut reports = totals(1, 6);
        reports.extend([(10, used(1_200, 60, 360)), (11, used(900, 40, 370))]);
        let result = effective(&[reports]);
        let turns: Vec<u64> = result[0].iter().map(Usage::total).collect();
        assert_eq!(turns, [1_100, 1_050, 1_050, 1_050, 1_050, 1_050, 1_260, 940]);
    }

    #[test]
    fn settle_rewrites_history_when_the_member_turns_out_to_report_totals() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE project_attempts (id TEXT, agent_id TEXT, native_thread_id TEXT, created_at INTEGER, status TEXT, usage TEXT, reported_usage TEXT);",
            )
            .unwrap();
        for (index, (_, usage)) in totals(0, 6).iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO project_attempts VALUES (?1,'claude-win-2','s',?2,'completed',?3,NULL)",
                    params![format!("a{index}"), 1_000 + index as i64, serde_json::to_string(usage).unwrap()],
                )
                .unwrap();
        }
        settle(&connection, "claude-win", 10_000).unwrap();
        // 再跑一遍结果不变：按原始报数重算，不会对已经改过的再减一次。
        settle(&connection, "claude-win", 10_000).unwrap();
        let stored: Vec<u64> = connection
            .prepare("SELECT usage FROM project_attempts ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|text| serde_json::from_str::<Usage>(&text.unwrap()).unwrap().total())
            .collect();
        assert_eq!(stored, [1_100, 1_050, 1_050, 1_050, 1_050, 1_050]);
    }
}

/// 手动核对：`USAGE_DB=<库副本> cargo test -- --ignored usage_mode_live --nocapture`，在副本上重算并列出改了的记录。
#[cfg(test)]
#[test]
#[ignore]
fn usage_mode_live() {
    let connection = Connection::open(std::env::var("USAGE_DB").unwrap()).unwrap();
    let before: std::collections::HashMap<String, String> = connection
        .prepare("SELECT id,usage FROM project_attempts WHERE usage IS NOT NULL").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
    connection.execute("ALTER TABLE project_attempts ADD COLUMN reported_usage TEXT", []).ok();
    for agent in ["claude-win", "codex-win", "dsh-win", "hermes-win"] {
        settle(&connection, agent, crate::store::now()).unwrap();
    }
    let mut query = connection.prepare("SELECT id,agent_id,native_thread_id,usage FROM project_attempts WHERE usage IS NOT NULL ORDER BY created_at").unwrap();
    for row in query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?))).unwrap() {
        let (id, agent, session, after) = row.unwrap();
        if before[&id] != after {
            let total = |text: &str| serde_json::from_str::<Usage>(text).unwrap().total();
            if total(&before[&id]) != total(&after) {
                println!("{agent} {} {} → {}", &session.unwrap_or_default()[..8], total(&before[&id]), total(&after));
            }
        }
    }
}
