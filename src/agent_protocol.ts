import type { ProjectAttempt, Workflow } from './types';

/** Workflow JSON is control data, never a reasoning block or the reply body. */
export function projectReply(attempt: ProjectAttempt, job: Workflow) {
  if (attempt.activity === 'discussion') return { body: attempt.output, protocol: undefined };
  if (attempt.stage === 'plan') {
    const plan = attempt.status === 'completed' ? job.plan : null;
    const body = plan ? [plan.summary, ...plan.tasks.map((task, index) => `${index + 1}. ${task.title}`)].join('\n\n') : attempt.status === 'running' || attempt.status === 'starting' ? '正在整理项目方案…' : '';
    return { body, protocol: attempt.output ? { label: '方案数据', raw: attempt.output } : undefined };
  }
  if (attempt.stage === 'review') {
    let body = '';
    try {
      const raw = attempt.output;
      const value = JSON.parse(raw.slice(raw.indexOf('{'), raw.lastIndexOf('}') + 1));
      if (typeof value.approved === 'boolean' && typeof value.summary === 'string' && Array.isArray(value.issues) && value.issues.every((issue: unknown) => typeof issue === 'string')) {
        body = [value.approved ? '验收通过' : '验收发现问题', value.summary, ...value.issues.map((issue: string) => `- ${issue}`)].join('\n\n');
      }
    } catch { /* Keep nonstandard control output in its raw-data disclosure. */ }
    if (!body && job.summary && attempt.status === 'completed' && [...job.attempts].reverse().find(item => item.stage === 'review')?.id === attempt.id) body = job.summary;
    if (!body && (attempt.status === 'running' || attempt.status === 'starting')) body = '正在检查项目结果…';
    return { body, protocol: attempt.output ? { label: '验收数据', raw: attempt.output } : undefined };
  }
  return { body: attempt.output, protocol: undefined };
}
