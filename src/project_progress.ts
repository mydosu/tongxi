import type { CapabilityRow, ProjectAgentChat, ProjectAttempt, ProjectQuestion, Workflow } from './types';
import { workflowBusy } from './project_ui';

type Escape = (value: string | number) => string;
const statuses: Record<string,string> = { queued:'等待执行',planning:'正在规划',running:'正在执行',reviewing:'正在验收',verifying:'正在验收',cancelling:'正在停止',interrupted:'已暂停',failed:'需要继续处理',completed:'已完成' };
const purposes:Record<string,string>={solution:'正在出方案',implementation:'正在实现',verification:'正在验证',closeout:'正在收尾',reply:'正在回复'};
const managerStatuses:Record<string,string>={routing:'正在调度',waiting_members:'等待成员结果',awaiting_user:'等待你的回答',paused:'已暂停',resting:'额度不足，暂停中（到点自动继续）',failed:'等待继续处理',finished:'本轮协调完成'};
const stages: Record<string,string> = { plan:'正在规划',implement:'正在执行',review:'正在验收',repair:'正在修复' };

/// token 数写成 12.3k / 1.2M。
export const tokens = (value: number) => value >= 1e6 ? `${(value / 1e6).toFixed(value >= 1e7 ? 0 : 1)}M` : value >= 1e3 ? `${(value / 1e3).toFixed(value >= 1e4 ? 0 : 1)}k` : String(value);
const known = (attempt: ProjectAttempt) => attempt.usage && !attempt.usage.context_only ? attempt.usage : null;
const spent = (attempts: ProjectAttempt[]) => attempts.reduce((sum, attempt) => { const usage = known(attempt); return sum + (usage ? usage.input + usage.output : 0); }, 0);

/// 给用户看的统计按 agent 和模型分，不按会话名：Codex 1号、方案、验证都是 Codex。
const agentLabels: Record<string,string> = { 'codex-win':'Codex', 'claude-win':'Claude', 'dsh-win':'DSH', 'hermes-win':'Hermes', 'albion-wsl':'阿尔比恩' };
export const agentLabel = (agent: string) => agentLabels[agent.replace(/-\d+$/, '')] || agent;
/// 模型名去掉线路前缀：DSH 的 ["线路","模型"]、Hermes 的 custom:线路:模型 都只留模型。
export const modelLabel = (model: string | null | undefined) => {
  if (!model) return '默认模型';
  try { const parts = JSON.parse(model); if (Array.isArray(parts) && parts.length === 2) return String(parts[1]); } catch { /* 不是 JSON 就按原样处理 */ }
  return model.startsWith('custom:') ? model.split(':').slice(2).join(':') || model : model;
};
const categories: Record<string,string> = { architecture:'架构设计', feature:'功能实现', bugfix:'修 bug', algorithm:'复杂算法', refactor:'重构', testing:'测试验证', docs:'文档', ops:'环境运维', integration:'多模块集成', uncategorized:'未分类' };
const percentOf = (value: number | null) => value == null ? '—' : `${Math.round(value * 100)}%`;

/// 失败执行按原因归类，与 capability.rs 的 failure_cause 保持一致。
const quotaWords = ['限流', '额度', 'rate limit', 'rate_limit', 'ratelimit', 'usage limit', 'quota', 'too many requests', '429'];
export function failureCause(attempt: ProjectAttempt): string | null {
  if (attempt.status !== 'failed' && attempt.status !== 'interrupted') return null;
  const error = (attempt.error || '').toLowerCase(), has = (words: string[]) => words.some(word => error.includes(word));
  if (error.includes('超出预算')) return 'budget';
  if (error.includes('请求升档') || error.includes('原地打转')) return 'escalate';
  if (attempt.status === 'interrupted' || has(['中断', '已停止', '超时', '超过当前工作流剩余时间', 'timeout'])) return 'interrupted';
  if (has(quotaWords)) return 'quota';
  if (has(['拒绝请求', '免费档', '-32603', '401', '认证', '余额', 'unavailable', 'overloaded'])) return 'provider';
  return 'other';
}
const causes: Record<string,string> = { quota:'撞限额', provider:'线路报错', budget:'超预算叫停', escalate:'请求升档', interrupted:'被中断', other:'其他' };

/// 白花的额度：失败执行的消耗按原因分，任务后来做完了的单列（工作树留着、被接手，不全算白花）。
function wasteRow(job: Workflow, escape: Escape) {
  const failed = job.attempts.filter(attempt => failureCause(attempt) && known(attempt));
  const total = spent(failed);
  if (!total) return '';
  const all = spent(job.attempts.filter(attempt => attempt.status !== 'starting'));
  const parts = Object.keys(causes).map(cause => [cause, spent(failed.filter(attempt => failureCause(attempt) === cause))] as const).filter(([, value]) => value).map(([cause, value]) => `${causes[cause]} ${tokens(value)}`).join(' · ');
  const done = new Set(job.tasks.filter(task => task.status === 'completed').map(task => task.id));
  const kept = spent(failed.filter(attempt => attempt.task_id && done.has(attempt.task_id)));
  return `<p class="usage-waste">失败执行用掉 <strong>${escape(tokens(total))}</strong>（占 ${all ? Math.round(total / all * 100) : 0}%）：${escape(parts)}${kept ? `；其中 ${escape(tokens(kept))} 的任务后来接着做完了` : ''}</p>`;
}

/// 用量明细：token 构成、调度占比、各模型消耗、统计完整度（只统计 token，不折成金额）。
function usageDetail(job: Workflow, escape: Escape) {
  const finished = job.attempts.filter(attempt => attempt.status !== 'starting');
  const usages = finished.map(known).filter((usage): usage is NonNullable<typeof usage> => !!usage);
  if (!usages.length) return '';
  const sum = (pick: (usage: NonNullable<ReturnType<typeof known>>) => number) => usages.reduce((total, usage) => total + pick(usage), 0);
  const read = sum(usage => usage.cached), write = sum(usage => usage.cache_write || 0);
  const fresh = Math.max(0, sum(usage => usage.input) - read - write), output = sum(usage => usage.output), reasoning = sum(usage => usage.reasoning);
  const all = spent(finished), manager = spent(finished.filter(attempt => attempt.activity === 'coordinate'));
  const cell = (label: string, value: string) => `<div><small>${label}</small><strong>${escape(value)}</strong></div>`;
  const key = (attempt: ProjectAttempt) => `${agentLabel(attempt.agent_id)}\u0000${modelLabel(attempt.model)}`;
  const groups = [...new Set(finished.filter(attempt => attempt.activity !== 'coordinate').map(key))];
  const rows = groups.map(group => {
    const [agent, model] = group.split('\u0000');
    const mine = finished.filter(attempt => attempt.activity !== 'coordinate' && key(attempt) === group);
    const unknown = mine.filter(attempt => !known(attempt)).length;
    return `<tr><td>${escape(model)}<small>${escape(agent)}</small></td><td>${escape(tokens(spent(mine)))}</td><td>${unknown ? `${unknown} 次未知` : '完整'}</td></tr>`;
  }).join('');
  return `<details class="project-usage-detail" data-keep="usage"><summary>用量明细</summary><div class="usage-breakdown">${cell('新输入', tokens(fresh))}${cell('缓存读取', tokens(read))}${cell('缓存写入', tokens(write))}${cell('输出', tokens(output))}${reasoning ? cell('其中推理', tokens(reasoning)) : ''}${cell('调度占比', all ? `${Math.round(manager / all * 100)}%` : '—')}${cell('统计完整度', `${finished.filter(known).length}/${finished.length}`)}</div>${wasteRow(job, escape)}${rows ? `<table><thead><tr><th>模型</th><th>token</th><th>完整度</th></tr></thead><tbody>${rows}</tbody></table>` : ''}</details>`;
}

type QuotaWindow = { used_percent: number; resets_in_hours?: number; used_usd?: number; cap_usd?: number; exceeded?: boolean };
export type QuotaEntry = { five_hour?: QuotaWindow; weekly?: QuotaWindow; monthly?: QuotaWindow; credits_left_usd?: number; account?: string; cancel_at?: string; period_end?: string; period?: { spent_usd?: number; tokens_in?: number; tokens_out?: number } };

/// 套餐额度：订阅看 5 小时 / 每周窗口；GOAT 线路另有本月额度、剩余额度，同一账号的线路标出来。
export function quotaTable(quota: Record<string, QuotaEntry>, escape: Escape) {
  const entries = Object.entries(quota);
  if (!entries.length) return '';
  const accounts = new Map<string, number>();
  entries.forEach(([, entry]) => { if (entry.account && !accounts.has(entry.account)) accounts.set(entry.account, accounts.size + 1); });
  const shared = (account?: string) => account && entries.filter(([, entry]) => entry.account === account).length > 1;
  const cell = (window?: QuotaWindow) => window ? `${window.used_percent}%${window.resets_in_hours != null ? `<small>${window.resets_in_hours} 小时后重置</small>` : ''}` : '—';
  const date = (text?: string) => text ? text.slice(5, 10).replace('-', '/') : '';
  const rows = entries.map(([key, entry]) => {
    const [agent, line] = key.split(':');
    const notes = [shared(entry.account) ? `账号 ${accounts.get(entry.account!)}（与其他线路共用）` : entry.account ? `账号 ${accounts.get(entry.account)}` : '', entry.cancel_at ? `套餐 ${date(entry.cancel_at)} 停止` : entry.period_end ? `${date(entry.period_end)} 续期` : '', entry.period?.spent_usd != null ? `本期已用 $${entry.period.spent_usd}` : ''].filter(Boolean).join(' · ');
    return `<tr><td>${escape(agentLabel(agent))}${line ? `<small>${escape(line)}</small>` : ''}</td><td>${cell(entry.five_hour)}</td><td>${cell(entry.weekly)}</td><td>${entry.monthly ? `${entry.monthly.used_percent}%` : '—'}</td><td>${entry.credits_left_usd != null ? `$${entry.credits_left_usd.toFixed(2)}` : '—'}</td><td>${escape(notes || '—')}</td></tr>`;
  }).join('');
  return `<details class="project-capability" data-keep="quota"><summary>套餐额度</summary><table><thead><tr><th>agent / 线路</th><th>5 小时</th><th>每周</th><th>本月</th><th>剩余额度</th><th>备注</th></tr></thead><tbody>${rows}</tbody></table><p>百分比是已用比例。GOAT 的金额是套餐额度；剩余额度含另购的部分。数据每 5 分钟最多刷新一次。</p></details>`;
}

/// 历史表现：各组合在不同任务类别上的平滑成功率、一次通过率、完整任务消耗与返修。
export function capabilityTable(rows: CapabilityRow[], escape: Escape) {
  if (!rows.length) return '';
  const confidence: Record<string,string> = { low:'样本少', medium:'中', high:'高' };
  const body = rows.map(row => `<tr><td title="${escape(row.model)}">${escape([row.model === 'default' ? '默认模型' : modelLabel(row.model), row.effort === 'default' ? '' : row.effort].filter(Boolean).join(' · '))}<small>${escape(agentLabel(row.agent))}</small></td><td>${escape(categories[row.category] || row.category)}</td><td>${row.samples}</td><td>${percentOf(row.success_rate)}</td><td>${percentOf(row.first_pass_rate)}</td><td>${row.avg_task_tokens == null ? '—' : escape(tokens(row.avg_task_tokens))}</td><td>${row.avg_repairs ?? '—'}</td><td>${escape(confidence[row.confidence])}</td></tr>`).join('');
  return `<details class="project-capability" data-keep="capability"><summary>历史表现（近一年）</summary><table><thead><tr><th>组合</th><th>类别</th><th>样本</th><th>成功率</th><th>一次通过</th><th>完整任务 token</th><th>平均返修</th><th>可信度</th></tr></thead><tbody>${body}</tbody></table><p>成功率是平滑后的估计：样本少时向 70% 收缩，撞限额、被停下或中断不计入；完整任务 token 含由它开头的返修和验证。调度选人时参考这张表。</p></details>`;
}

/// 本轮用量：成员干活与调度分开；有成员没报用量（如 Hermes）就注明「不全」。
function usageRow(job: Workflow, escape: Escape) {
  const finished = job.attempts.filter(attempt => attempt.status !== 'starting');
  if (!finished.length) return '';
  const coordinator = finished.filter(attempt => attempt.activity === 'coordinate');
  const members = finished.filter(attempt => attempt.activity !== 'coordinate');
  const unknown = finished.filter(attempt => !known(attempt)).length;
  return `<div class="project-usage" title="确知的实际 token 消耗（输入含缓存读写，输出含推理）。成员只报了上下文占用或没报用量时不计入，记为未知。"><span>本轮用量</span><strong>${escape(tokens(spent(members)))}</strong><small>成员</small><strong>${escape(tokens(spent(coordinator)))}</strong><small>调度</small>${unknown ? `<em>${unknown} 次用量未知，未计入</em>` : ''}</div>`;
}

export function progressContent(jobs: Workflow[], members: ProjectAgentChat[], escape: Escape, name: (agent: string) => string, avatar: (agent: string) => string, questions:ProjectQuestion[] = [], capability: CapabilityRow[] = [], quota: Record<string, QuotaEntry> = {}) {
  const current=jobs.find(job=>workflowBusy(job)) || jobs[0];
  const completed=current?.tasks.filter(task=>task.status==='completed').length || 0;
  const total=current?.tasks.length || 0;
  // Only a finished goal shows 100%; dynamic routing may still add follow-up work.
  const percent=current?.status==='completed' ? 100 : total ? current?.coordination ? Math.min(99, Math.round(completed/total*100)) : Math.round(completed/(total+1)*100) : 0;
  const roster=members.map(member=>{
    const attempts=jobs.flatMap(job=>[...job.attempts].reverse()).filter(attempt=>attempt.agent_id===member.agent_id&&attempt.activity!=='coordinate');
    const active=attempts.find(attempt=>['starting','running','cancelling'].includes(attempt.status));
    const last=attempts[0];
    const waiting=questions.some(question=>question.agent_id===member.agent_id&&question.status==='pending'&&!current?.attempts.some(a=>a.id===question.attempt_id&&a.activity==='coordinate'));
    // 任务开工前（建工作树、起会话）和做完后（合并结果）都还没有在跑的执行记录，但任务仍在进行，灯也要亮。
    const task=!active&&current&&workflowBusy(current) ? current.tasks.find(item=>item.status==='running'&&item.agent_id===member.agent_id) : undefined;
    const between=task ? (current!.attempts.some(attempt=>attempt.task_id===task.id&&attempt.status==='completed') ? '正在合并结果' : '正在准备') : '';
    const status=waiting ? '等待你的回答' : active ? active.status==='cancelling'?'正在停止':active.activity==='discussion'?'正在交流':active.activity==='resolve:git'?'正在处理合并冲突':purposes[active.activity?.replace('task:','')||'']||stages[active.stage] || '正在工作' : between || (last?.status==='failed' ? '等待继续处理' : last?.status==='interrupted' ? '已暂停' : '空闲');
    const light=waiting ? 'waiting' : active||task ? 'working' : last?.status==='failed' ? 'failed' : '';
    return `<button class="project-member-status ${light}" data-progress-agent="${escape(member.agent_id)}">${avatar(member.agent_id)}<div><strong>${escape(name(member.agent_id))}</strong><span>${escape(status)}</span></div><i></i></button>`;
  }).join('');
  const manager=current?.coordination;
  const managerRow=manager?`<div class="project-coordinator-status" data-project-coordinator-status data-status="${escape(manager.status)}">${avatar(manager.coordinator.agent)}<div><strong>${escape(name(manager.coordinator.agent))} · 调度管家</strong><span>${escape(managerStatuses[manager.status]||manager.status)}</span></div><small>独立调度会话</small></div>`:'';
  const tasks=current?.tasks.map(task=>{
    const route=current.coordination?.routes.find(item=>item.task_id===task.id);
    const used=spent(current.attempts.filter(attempt=>attempt.task_id===task.id));
    const setup=[task.model,task.effort].filter(Boolean).join(' · ');
    const detail=[name(task.agent_id),({queued:'等待执行',running:'正在执行',completed:'已完成',interrupted:'已暂停',failed:'需要处理',skipped:'已取消'} as Record<string,string>)[task.status] || task.status,setup,used?`${tokens(used)} token`:''].filter(Boolean).join(' · ');
    return `<li${route?.instructions?` title="${escape(`调度的任务说明：\n${route.instructions}`)}"`:''}><span class="progress-task-mark ${escape(task.status)}">${task.status==='completed'?'✓':'·'}</span><div><strong>${escape(task.title)}</strong><small>${escape(detail)}</small>${route?.reason?`<small class="progress-task-reason">${escape(route.reason)}</small>`:''}</div></li>`;
  }).join('') || '';
  const ledger=current?.coordination?.ledger?`<details class="project-ledger"><summary>调度的计划账本</summary><p>${escape(current.coordination.ledger)}</p></details>`:'';
  return `<section class="project-progress-overview"><div class="project-progress-heading"><div><span class="eyebrow">当前协作</span><h2>${escape(current ? statuses[current.status] || current.status : '等待新的项目目标')}</h2></div><strong>${percent}<small>%</small></strong></div><div class="project-progress-meter" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${percent}"><span style="width:${percent}%"></span></div><p>${total?`${completed} / ${total} 项工作已完成`:'发送需求后，团队会在这里同步进度。'}</p></section>${managerRow}<div class="project-member-status-grid">${roster}</div>${current ? `<section class="project-progress-work" data-workflow-id="${escape(current.id)}"><header><h3>工作安排</h3><span>${escape(statuses[current.status] || current.status)}</span>${workflowBusy(current)?`<button class="text-button" data-stop-project="${escape(current.id)}">暂停协作</button>`:['failed','interrupted'].includes(current.status)?`<button class="text-button" data-continue-project="${escape(current.id)}">继续协作</button>`:''}</header>${tasks?`<ol>${tasks}</ol>`:'<p>团队正在整理本轮目标。</p>'}${ledger}${usageRow(current,escape)}${usageDetail(current,escape)}${current.error?`<p class="workflow-error">${escape(current.error)}</p>`:''}</section>`:''}${quotaTable(quota,escape)}${capabilityTable(capability,escape)}`;
}

export function projectProgressPage(project: string, content: string, escape: Escape) {
  return `<header class="chat-header"><div><h1>项目协作进度</h1><p>${escape(project)} · 成员状态与工作进度</p></div><div class="header-actions"><button class="secondary" data-project-configure>项目成员配置</button><button class="secondary" data-project-sessions>会话绑定</button><button class="secondary" data-project-return-team>团队对话</button></div></header><div class="project-progress-content">${content}</div>`;
}
