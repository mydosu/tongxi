import type { ProjectAgentChat, ProjectQuestion, Workflow } from './types';
import { workflowBusy } from './project_ui';

type Escape = (value: string | number) => string;
const statuses: Record<string,string> = { queued:'等待执行',planning:'正在规划',running:'正在执行',reviewing:'正在验收',verifying:'正在验收',cancelling:'正在停止',interrupted:'已暂停',failed:'需要继续处理',completed:'已完成' };
const purposes:Record<string,string>={solution:'正在出方案',implementation:'正在实现',verification:'正在验证',closeout:'正在收尾',reply:'正在回复'};
const managerStatuses:Record<string,string>={routing:'正在调度',waiting_members:'等待成员结果',awaiting_user:'等待你的回答',paused:'已暂停',failed:'等待继续处理',finished:'本轮协调完成'};
const stages: Record<string,string> = { plan:'正在规划',implement:'正在执行',review:'正在验收',repair:'正在修复' };

export function progressContent(jobs: Workflow[], members: ProjectAgentChat[], escape: Escape, name: (agent: string) => string, avatar: (agent: string) => string, questions:ProjectQuestion[] = []) {
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
    const status=waiting ? '等待你的回答' : active ? active.status==='cancelling'?'正在停止':active.activity==='discussion'?'正在交流':active.activity==='resolve:git'?'正在处理合并冲突':purposes[active.activity?.replace('task:','')||'']||stages[active.stage] || '正在工作' : last?.status==='failed' ? '等待继续处理' : last?.status==='interrupted' ? '已暂停' : '空闲';
    return `<button class="project-member-status ${active?'working':''}" data-progress-agent="${escape(member.agent_id)}">${avatar(member.agent_id)}<div><strong>${escape(name(member.agent_id))}</strong><span>${escape(status)}</span></div><i></i></button>`;
  }).join('');
  const manager=current?.coordination;
  const managerRow=manager?`<div class="project-coordinator-status" data-project-coordinator-status data-status="${escape(manager.status)}">${avatar(manager.coordinator.agent)}<div><strong>${escape(name(manager.coordinator.agent))} · 调度管家</strong><span>${escape(managerStatuses[manager.status]||manager.status)}</span></div><small>独立调度会话</small></div>`:'';
  const tasks=current?.tasks.map(task=>`<li><span class="progress-task-mark ${escape(task.status)}">${task.status==='completed'?'✓':'·'}</span><div><strong>${escape(task.title)}</strong><small>${escape(name(task.agent_id))} · ${escape(({queued:'等待执行',running:'正在执行',completed:'已完成',interrupted:'已暂停',failed:'需要处理',skipped:'已取消'} as Record<string,string>)[task.status] || task.status)}</small></div></li>`).join('') || '';
  return `<section class="project-progress-overview"><div class="project-progress-heading"><div><span class="eyebrow">当前协作</span><h2>${escape(current ? statuses[current.status] || current.status : '等待新的项目目标')}</h2></div><strong>${percent}<small>%</small></strong></div><div class="project-progress-meter" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${percent}"><span style="width:${percent}%"></span></div><p>${total?`${completed} / ${total} 项工作已完成`:'发送需求后，团队会在这里同步进度。'}</p></section>${managerRow}<div class="project-member-status-grid">${roster}</div>${current ? `<section class="project-progress-work" data-workflow-id="${escape(current.id)}"><header><h3>工作安排</h3><span>${escape(statuses[current.status] || current.status)}</span>${workflowBusy(current)?`<button class="text-button" data-stop-project="${escape(current.id)}">暂停协作</button>`:['failed','interrupted'].includes(current.status)?`<button class="text-button" data-continue-project="${escape(current.id)}">继续协作</button>`:''}</header>${tasks?`<ol>${tasks}</ol>`:'<p>团队正在整理本轮目标。</p>'}${current.error?`<p class="workflow-error">${escape(current.error)}</p>`:''}</section>`:''}`;
}

export function projectProgressPage(project: string, content: string, escape: Escape) {
  return `<header class="chat-header"><div><h1>项目协作进度</h1><p>${escape(project)} · 成员状态与工作进度</p></div><div class="header-actions"><button class="secondary" data-project-configure>项目成员配置</button><button class="secondary" data-project-sessions>会话绑定</button><button class="secondary" data-project-return-team>团队对话</button></div></header><div class="project-progress-content">${content}</div>`;
}
