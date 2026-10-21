import type { Inquiry } from './types';

type Escape = (value: string | number) => string;

/// 「3 小时前」「2 天前」：问题挂了多久。
function ago(at: number) {
  const minutes = Math.max(0, Math.round((Date.now() - at) / 60000));
  if (minutes < 60) return minutes <= 1 ? '刚刚' : `${minutes} 分钟前`;
  const hours = Math.round(minutes / 60);
  return hours < 24 ? `${hours} 小时前` : `${Math.round(hours / 24)} 天前`;
}

function openCard(item: Inquiry, escape: Escape, name: (agent: string) => string) {
  const meanwhile = item.reversible
    ? item.default_choice ? `回答前先按「${escape(item.default_choice)}」推进，能回退` : '回答前按调度的判断推进，能回退'
    : '不可逆，回答前相关的活先搁着';
  const options = item.options.map(option => `<button type="button" class="inquiry-option" data-inquiry-option="${escape(option)}">${escape(option)}</button>`).join('');
  return `<article class="inquiry-card ${item.reversible ? '' : 'held'}" data-inquiry-id="${escape(item.id)}">
    <header><strong>${escape(item.question)}</strong><small>${escape(item.asked_by === 'coordinator' ? '调度' : name(item.asked_by))} · ${escape(ago(item.created_at))}</small></header>
    <p class="inquiry-meanwhile">${meanwhile}</p>
    ${item.background ? `<details class="inquiry-background"><summary>为什么问</summary><p>${escape(item.background)}</p></details>` : ''}
    ${options ? `<div class="inquiry-options">${options}</div>` : ''}
    <form class="inquiry-answer" data-inquiry-form="${escape(item.id)}"><textarea rows="2" placeholder="写下你的回答，可以补充条件或理由" required></textarea><button type="submit" class="primary">回答</button></form>
  </article>`;
}

function answeredRow(item: Inquiry, escape: Escape) {
  return `<li><strong>${escape(item.question)}</strong><span>${escape(item.answer || '')}</span><small>${item.answered_at ? escape(ago(item.answered_at)) : ''}${item.seen ? ' · 调度已看到' : ' · 等调度下一回合看到'}</small></li>`;
}

/// 协作问答：调度要你确认的事。没答的在上面，答过的收在下面。
export function inquiryPage(project: string, items: Inquiry[], escape: Escape, name: (agent: string) => string) {
  const open = items.filter(item => item.status === 'open');
  const answered = items.filter(item => item.status === 'answered');
  const body = open.length
    ? open.map(item => openCard(item, escape, name)).join('')
    : '<p class="inquiry-empty">现在没有要你确认的事。调度有问题时会挂在这里，不会刷进群聊。</p>';
  return `<header class="chat-header"><div><h1>协作问答</h1><p>${escape(project)} · 调度要你确认的事，什么时候答都行</p></div><div class="header-actions"><button class="secondary" data-project-return-team>团队对话</button></div></header>
  <div class="project-progress-content inquiry-content">${body}${answered.length ? `<details class="inquiry-answered" data-keep="answered"><summary>已回答（${answered.length}）</summary><ul>${answered.map(item => answeredRow(item, escape)).join('')}</ul></details>` : ''}</div>`;
}
