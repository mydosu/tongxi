import type { ConversationDetail, RuntimeSnapshot } from './types';
import { modelOptionsHtml, modelProviders, providerOptionsHtml, selectedProvider } from './model_select';

type Escape = (value: string | number) => string;
const efforts: Record<string, string> = { none: '关闭思考', off: '关闭思考', minimal: '最少', low: '低', medium: '中', high: '高', xhigh: '很高', max: '最大', ultra: 'Ultra' };

export function projectAgentPage(name: string, portrait: string, project: string, agent: string, chat: string, escape: Escape) {
  return `<header class="chat-header project-agent-room-header"><div class="chat-heading">${portrait}<div><h1>${escape(name)}</h1><p>${escape(project)} · 专属会话</p></div></div><div class="header-actions"><span id="project-agent-connection" class="connection-badge"></span><button id="connect-project-agent" class="secondary">连接 ${escape(name)}</button><button id="project-team-return" class="secondary">团队对话</button></div></header><section class="project-agent-room-body"><div class="project-agent-chat" data-project-chat-id="${escape(chat)}" data-project-chat-agent="${escape(agent)}"><p class="team-feed-empty">正在载入会话…</p></div></section>`;
}

export function agentChatChrome(chat: ConversationDetail, agent: string, name: string, runtime: RuntimeSnapshot, escape: Escape, arrow: string) {
  const settings = chat.sessions.find(session => session.agent_id === agent);
  const models = runtime.models;
  const providers = modelProviders(models);
  const effective = settings?.model || runtime.default_model;
  const provider = selectedProvider(models, effective);
  const choices = models.find(model => model.id === effective)?.efforts || Object.keys(efforts);
  const defaultLabel = `默认${runtime.default_model ? ` · ${models.find(model => model.id === runtime.default_model)?.name || runtime.default_model}` : '模型'}`;
  return `<div class="project-agent-chat-settings ${providers.length > 1 ? 'multi-provider' : ''}">${providers.length > 1 ? `<label>提供商<select data-project-chat-provider aria-label="项目私聊提供商">${providerOptionsHtml(models, provider, escape)}</select></label>` : ''}<label class="project-chat-model-field">模型<select data-project-chat-model aria-label="项目私聊模型">${modelOptionsHtml(models, provider, settings?.model || '', escape, defaultLabel)}</select></label><label class="project-chat-effort-field">思考强度<select data-project-chat-effort aria-label="项目私聊思考强度"><option value="">默认强度</option>${choices.map(effort => `<option value="${escape(effort)}" ${effort === settings?.reasoning_effort ? 'selected' : ''}>${escape(efforts[effort] || effort)}</option>`).join('')}</select></label></div><div class="project-agent-chat-heading"><span class="project-chat-settings-note">切换即保存 · 下一条消息生效</span></div><div class="project-agent-chat-messages" role="log" aria-label="${escape(name)} 的会话输出"></div><form class="project-agent-chat-composer" data-project-agent-chat-form><textarea data-project-agent-chat-input maxlength="16000" rows="2" aria-label="单独发消息给 ${escape(name)}" placeholder="发消息给 ${escape(name)}…"></textarea><div class="project-chat-composer-bottom"><span>Enter 发送 · Shift + Enter 换行</span><button type="submit" class="primary" data-project-agent-chat-send>发送${arrow}</button></div></form>`;
}

export function agentEmpty(name: string, escape: Escape) {
  return `<div class="project-agent-chat-empty"><h2>与 ${escape(name)} 一起工作</h2><p>在这里直接交给它一项任务。<br>项目执行的思考、工具和回复也会显示在这条会话中。</p></div>`;
}
