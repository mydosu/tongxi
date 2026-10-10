type Escape = (value: string | number) => string;


export function projectAgentPage(name: string, portrait: string, project: string, agent: string, chat: string, escape: Escape) {
  return `<header class="chat-header project-agent-room-header"><div class="chat-heading">${portrait}<div><h1>${escape(name)}</h1><p>${escape(project)} · 专属会话</p></div></div><div class="header-actions"><span id="project-agent-connection" class="connection-badge"></span><button id="connect-project-agent" class="secondary">连接 ${escape(name)}</button><button id="project-team-return" class="secondary">团队对话</button></div></header><section class="project-agent-room-body"><div class="project-agent-chat" data-project-chat-id="${escape(chat)}" data-project-chat-agent="${escape(agent)}"><p class="team-feed-empty">正在载入会话…</p></div></section>`;
}

export function agentChatChrome(name: string, escape: Escape, arrow: string) {
  return `<div class="project-agent-chat-heading"><span class="project-chat-settings-note">本项目会话 · 模型由项目成员配置统一管理</span></div><div class="project-agent-chat-messages" role="log" aria-label="${escape(name)} 的会话输出"></div><form class="project-agent-chat-composer" data-project-agent-chat-form><textarea data-project-agent-chat-input maxlength="16000" rows="2" aria-label="单独发消息给 ${escape(name)}" placeholder="发消息给 ${escape(name)}…"></textarea><div class="project-chat-composer-bottom"><span>Enter 发送 · Shift + Enter 换行</span><button type="submit" class="primary" data-project-agent-chat-send>发送${arrow}</button></div></form>`;
}

export function agentEmpty(name: string, escape: Escape) {
  return `<div class="project-agent-chat-empty"><h2>与 ${escape(name)} 一起工作</h2><p>在这里直接交给它一项任务。<br>项目执行的思考、工具和回复也会显示在这条会话中。</p></div>`;
}
