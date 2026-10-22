import type { BrainJob, BrainOverview, PlanDoc } from './types';
import { agentMarkdown } from './agent_markdown';

type Escape = (value: string | number) => string;

/// 「4 分钟」「2 小时 5 分」：一件活做了多久（没做完就算到现在）。
function span(from: number, to: number) {
  const minutes = Math.max(0, Math.round((to - from) / 60000));
  if (minutes < 1) return '不到 1 分钟';
  if (minutes < 60) return `${minutes} 分钟`;
  const hours = Math.floor(minutes / 60), rest = minutes % 60;
  return hours < 24 ? `${hours} 小时${rest ? ` ${rest} 分` : ''}` : `${Math.round(hours / 24)} 天`;
}

function when(at: number) {
  return new Date(at).toLocaleString('zh-CN', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' });
}

export const jobLive = (job: BrainJob) => job.status === 'pending' || job.status === 'running';

function state(job: BrainJob): [string, string] {
  if (job.status === 'pending') return ['排队', 'pending'];
  if (job.status === 'running') return ['在做', 'running'];
  if (job.status === 'done') return ['做完了', 'done'];
  if (job.run_status === 'interrupted' || job.error === '你在派活页取消了') return ['被停下了', 'stopped'];
  return ['失败', 'failed'];
}

/// DSH 的模型存成 JSON 数组（线路 + 模型），拆开显示。
export function modelText(model: string | null) {
  if (!model) return '会话默认模型';
  try { const parts = JSON.parse(model); if (Array.isArray(parts)) return parts.join(' / '); } catch { /* 普通模型名 */ }
  return model;
}

function card(job: BrainJob, escape: Escape, avatar: (seat: string) => string, open: Set<string>) {
  const [label, tone] = state(job);
  const title = job.content.trim().split('\n')[0].slice(0, 120);
  const long = job.content.trim() !== title;
  const timing = jobLive(job) ? `${when(job.created_at)} 派出 · 已 ${span(job.created_at, Date.now())}` : `${when(job.created_at)} 派出 · 用时 ${span(job.created_at, job.finished_at || job.created_at)}`;
  const tree = job.branch ? `<span title="${escape(job.worktree || '')}">工作树 ${escape(job.branch)}${job.worktree ? '' : '（已合并或丢弃）'}</span>` : '<span>在项目目录里做</span>';
  // 任务原文照原样显示；交回的结果是成员的回复，按 Markdown 排版。
  const fold = (key: string, summary: string, body: string) => `<details data-job-fold="${escape(`${job.id}|${key}`)}" ${open.has(`${job.id}|${key}`) ? 'open' : ''}><summary>${summary}</summary><div class="brain-job-text${key === 'result' ? ' markdown' : ''}">${key === 'result' ? agentMarkdown(body, escape) : escape(body)}</div></details>`;
  return `<article class="brain-job ${tone}" data-job-id="${escape(job.id)}">
    <header>${avatar(job.seat)}<strong>${escape(job.name)}</strong><span class="brain-job-state ${tone}">${label}</span><small>${escape(timing)}</small></header>
    <p class="brain-job-title">${escape(title)}</p>
    ${long ? fold('task', '任务全文', job.content.trim()) : ''}
    <p class="brain-job-meta"><span>${escape(modelText(job.model))}${job.effort ? ` · ${escape(job.effort)}` : ''}</span>${tree}<span>任务 ${escape(job.id.slice(0, 8))}</span></p>
    ${job.error && job.error !== '你在派活页取消了' ? `<p class="brain-job-error">${escape(job.error)}</p>` : ''}
    ${job.result ? fold('result', '交回的结果', job.result) : ''}
    <div class="brain-job-actions">${jobLive(job) ? `<button type="button" class="secondary" data-stop-job="${escape(job.id)}">${job.status === 'pending' ? '取消' : '停止'}</button>` : ''}<button type="button" class="text-button" data-open-seat="${escape(job.seat)}">打开 ${escape(job.name)} 的会话</button></div>
  </article>`;
}

/// 任务书里的勾选框：数出做了几步，显示时换成看得懂的符号。
const STEP = /^(\s*(?:[-*+]|\d+\.)\s+)\[([ xX])\]/;
function planDoc(doc: PlanDoc, escape: Escape, open: Set<string>) {
  const lines = doc.text.split('\n');
  const steps = lines.map(line => line.match(STEP)?.[2]).filter(Boolean);
  const done = steps.filter(mark => mark !== ' ').length;
  const progress = steps.length ? `<span class="brain-plan-progress${done === steps.length ? ' done' : ''}">${done}/${steps.length}</span>` : '';
  const text = lines.map(line => line.replace(STEP, (_, head, mark) => `${head}${mark === ' ' ? '☐' : '☑'}`)).join('\n');
  const key = `plan|${doc.brief ? 'brief' : 'plan'}|${doc.name}`;
  return `<details class="brain-plan" data-job-fold="${escape(key)}" ${open.has(key) ? 'open' : ''}><summary><strong>${doc.brief ? `任务书 · ${escape(doc.name)}` : '长期计划'}</strong>${progress}<small>${escape(when(doc.modified))} 更新</small></summary><div class="brain-job-text markdown">${agentMarkdown(text, escape)}</div></details>`;
}

function plansSection(plans: PlanDoc[], escape: Escape, open: Set<string>) {
  const body = plans.length ? plans.map(doc => planDoc(doc, escape, open)).join('')
    : '<p class="inquiry-empty">主脑还没写计划。长或难的活它会先在 .tongxi/任务书/ 写任务书、边做边勾，要长期记住的写进 .tongxi/计划.md，都会显示在这里。</p>';
  return `<section class="brain-plans"><h2>计划与任务书</h2>${body}</section>`;
}

/// 主脑派活：主脑现在在干嘛、派出去的每件活。在做的在上面，做完的收在下面。
export function brainJobsPage(project: string, overview: BrainOverview | null, brainBusy: boolean, escape: Escape, avatar: (seat: string) => string, open: Set<string>) {
  const head = `<header class="chat-header"><div><h1>主脑派活</h1><p>${escape(project)} · 主脑的计划、交给成员的每件活</p></div><div class="header-actions"><button class="secondary" data-project-return-team>团队对话</button>${overview?.chat ? `<button class="secondary" data-open-seat="${escape(overview.seat)}">主脑会话</button>` : ''}</div></header>`;
  if (!overview) return `${head}<div class="project-progress-content brain-jobs-content"><p class="inquiry-empty">正在读取…</p></div>`;
  const live = overview.jobs.filter(jobLive), ended = overview.jobs.filter(job => !jobLive(job));
  const status = `<section class="brain-status"><span class="brain-status-dot ${brainBusy ? 'busy' : ''}"></span><strong>主脑${brainBusy ? '正在回复' : '空闲'}</strong><small>${live.length ? `${live.length} 件活在外面` : '没有在外面的活'}${overview.waiting ? ` · ${overview.waiting} 条结果或回答等着送进主脑` : ''}</small></section>`;
  const body = live.length
    ? live.map(job => card(job, escape, avatar, open)).join('')
    : `<p class="inquiry-empty">${ended.length ? '现在没有在做的活。' : '主脑还没派过活。在团队对话里说要做的事，主脑会把活派给成员，派出去的都列在这里。'}</p>`;
  const history = ended.length ? `<details class="brain-jobs-ended" data-job-fold="ended" ${open.has('ended') ? 'open' : ''}><summary>已结束（${ended.length}）</summary>${ended.map(job => card(job, escape, avatar, open)).join('')}</details>` : '';
  return `${head}<div class="project-progress-content brain-jobs-content">${status}${plansSection(overview.plans, escape, open)}${body}${history}</div>`;
}
