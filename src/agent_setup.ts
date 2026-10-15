import { invoke } from '@tauri-apps/api/core';
import { loadImage } from './image_file';

// 成员接入：扫描本机已装的 agent、启动时自动连上；没装的一键在终端里装（命令先给用户看），
// 装好接着做初次登录/配置。伴侣不需要安装，靠配置一个 OpenAI 兼容接口接入。
type Escape = (value: string | number) => string;
export interface AgentEnv { id: string; name: string; installed: boolean; executable: string | null; version: string | null; missing: string | null; install_command: string; setup_command: string; docs: string; }
export interface CompanionView { configured: boolean; name: string; subtitle: string; endpoint: string; key_source: 'none' | 'env' | 'file' | 'inline'; key_env: string | null; key_file: string | null; key_name: string | null; model: string | null; persona: string | null; avatar: string | null; has_secret: boolean; }
interface Host { escape: Escape; open: (title: string, note: string, body: string) => void; modal: HTMLDialogElement; toast: (text: string, error?: boolean) => void; }

const AUTO = 'hub.autoConnect';
let envs: AgentEnv[] = [];
export const autoConnectEnabled = () => { try { return localStorage.getItem(AUTO) !== '0'; } catch { return true; } };
export const setAutoConnect = (on: boolean) => { try { localStorage.setItem(AUTO, on ? '1' : '0'); } catch { /* 偏好存不下就算了 */ } };
export const scanned = () => envs;
export async function scanAgents() { envs = await invoke<AgentEnv[]>('scan_agents'); return envs; }

/// 启动时：扫一遍，已安装但没连上的成员逐个静默连接；伴侣配好了也连。失败不弹错，留给服务页显示。
export async function autoConnect(connected: (id: string) => boolean, connect: (id: string) => Promise<void>) {
  if (!autoConnectEnabled()) return;
  const found = await scanAgents().catch(() => []);
  for (const env of found) if (env.installed && !connected(env.id)) await connect(env.id).catch(() => {});
  const companion = await invoke<CompanionView>('companion_config').catch(() => null);
  if (companion?.configured && !connected('albion-wsl')) await connect('albion-wsl').catch(() => {});
}

export function setupBanner(escape: Escape) {
  const ready = envs.filter(env => env.installed).length;
  return `<section class="setup-center" id="setup-center"><div class="setup-copy"><span class="eyebrow">接入成员</span><strong>${envs.length ? `本机找到 ${ready}/${envs.length} 位成员` : '正在扫描本机…'}</strong><small>同席自动发现本机装好的 Codex、Claude Code、Hermes、DSH 并连上；没装的点「下载安装」，会在终端里跑官方安装命令，装好接着做登录。伴侣用你自己的服务接入。</small></div><div class="setup-actions"><label class="setup-auto"><input type="checkbox" id="auto-connect" ${autoConnectEnabled() ? 'checked' : ''}>启动时自动接入</label><button type="button" class="secondary" id="setup-rescan">重新扫描</button><button type="button" class="primary" id="setup-connect-all" ${ready ? '' : 'disabled'}>接入已安装的成员</button></div>${envs.length ? '' : `<p class="model-setting-note">${escape('扫描只读取安装位置和版本，不会连接或改动任何配置。')}</p>`}</section>`;
}

/// 每张成员卡上的安装状态（只是状态文字；安装、登录、说明这些操作在卡片按钮和「更多」里）。
export function setupLine(id: string, escape: Escape) {
  if (id === 'albion-wsl') return '<div class="setup-line installed"><span class="fact-label">接入</span><span>你自己的伴侣服务（OpenAI 兼容接口）</span></div>';
  const env = envs.find(item => item.id === id);
  if (!env) return '<div class="setup-line"><span class="fact-label">安装</span><span>扫描中…</span></div>';
  const state = env.installed ? `已安装${env.version ? ` · ${escape(env.version)}` : ''}` : '未安装';
  return `<div class="setup-line${env.installed ? ' installed' : ''}" title="${escape(env.executable || '')}"><span class="fact-label">安装</span><span>${state}${env.missing ? ` · <em>${escape(env.missing)}</em>` : ''}</span></div>`;
}

/// 安装前把要执行的命令给用户看，确认后才开终端。
export function confirmInstall(id: string, host: Host, done: () => void) {
  const env = envs.find(item => item.id === id);
  if (!env) return;
  host.open(`安装 ${env.name}`, '会打开一个终端窗口，按官方方式安装；装好后接着进入初次登录/配置。整个过程都在终端里，你能看到每一步。', `<div class="install-confirm"><span class="field-label">将执行</span><pre>${host.escape(env.install_command)}</pre>${env.missing ? `<p class="model-setting-note">${host.escape(env.missing)}</p>` : ''}<span class="field-label">装好后</span><pre>${host.escape(env.setup_command)}</pre><p class="model-setting-note">安装来自官方源（<a href="${host.escape(env.docs)}" target="_blank" rel="noreferrer">${host.escape(env.docs)}</a>）。完成后回到这里点「重新扫描」，同席会自动接上。</p><div class="modal-footer"><button type="button" class="secondary" data-install-cancel>取消</button><button type="button" class="primary" data-install-go>打开终端安装</button></div></div>`);
  host.modal.querySelector('[data-install-cancel]')!.addEventListener('click', () => host.modal.close());
  host.modal.querySelector('[data-install-go]')!.addEventListener('click', async () => {
    try { host.toast(await invoke<string>('install_agent', { agentId: id })); host.modal.close(); done(); }
    catch (error) { host.toast(String(error), true); }
  });
}

export async function runSetup(id: string, host: Host) {
  try { host.toast(await invoke<string>('setup_agent', { agentId: id })); }
  catch (error) { host.toast(String(error), true); }
}

/// 头像：缩到 256×256 的 webp。
export async function shrink(file: File) {
  const picture = await loadImage(file);
  {
    const canvas = document.createElement('canvas'); canvas.width = canvas.height = 256;
    const side = Math.min(picture.naturalWidth, picture.naturalHeight);
    canvas.getContext('2d')!.drawImage(picture, (picture.naturalWidth - side) / 2, (picture.naturalHeight - side) * .3, side, side, 0, 0, 256, 256);
    return canvas.toDataURL('image/webp', .85);
  }
}

export async function companionDialog(host: Host & { saved: () => Promise<void> }) {
  const { escape } = host;
  const config = await invoke<CompanionView>('companion_config');
  let avatar = config.avatar;
  host.open('配置伴侣', '每个人可以接入自己的情感伴侣。同席只当客户端：人格、记忆在你的服务里，或者写在下面的人设里。', `<form id="companion-form" class="companion-form">
    <details class="companion-guide" ${config.configured ? '' : 'open'}><summary>怎么接入？</summary><p>只要你的伴侣提供 <b>OpenAI 兼容接口</b>（<code>/v1/models</code> 和流式 <code>/v1/chat/completions</code>），就能接进来：</p><ul>
      <li><b>自建伴侣服务</b>（人格、记忆、工具都在服务里）：填服务地址，例如 <code>http://127.0.0.1:8650</code>，Key 按服务要求填；人设留空。</li>
      <li><b>本机模型</b>：Ollama 填 <code>http://127.0.0.1:11434/v1</code>，LM Studio 填 <code>http://127.0.0.1:1234/v1</code>，Key 选「不需要」，在下面写人设。</li>
      <li><b>云端模型</b>：DeepSeek 填 <code>https://api.deepseek.com</code>，OpenAI 填 <code>https://api.openai.com/v1</code>，其他兼容服务同理；Key 可以直接填写（用 Windows 账户加密保存在本机），也可以放在环境变量或 .env 文件里。</li>
    </ul><p>地址只有主机时会自动补 <code>/v1</code>。同席每轮把这段私聊的历史一起发过去，伴侣不会失忆；群聊和项目暂不叫伴侣。</p></details>
    <div class="companion-head"><label class="companion-avatar" title="点击更换头像"><input type="file" accept="image/*" hidden data-companion-avatar><span class="avatar large albion-portrait" data-avatar-preview></span><small>换头像</small></label><div class="companion-names"><label class="field">名字<input name="name" maxlength="20" required value="${escape(config.name)}" placeholder="例如：阿尔比恩"></label><label class="field">一句话介绍<input name="subtitle" maxlength="40" value="${escape(config.subtitle === '待配置' ? '' : config.subtitle)}" placeholder="例如：WSL 本机服务"></label></div></div>
    <label class="field">接口地址<input name="endpoint" required value="${escape(config.endpoint)}" placeholder="http://127.0.0.1:11434/v1"></label>
    <label class="field">Key<select name="key_source"><option value="none">不需要</option><option value="inline">直接填写（加密保存在本机）</option><option value="env">从环境变量读取</option><option value="file">从 .env 文件读取</option></select></label>
    <div class="companion-key" data-key="inline"><label class="field">Key<input name="secret" type="password" autocomplete="off" placeholder="${config.has_secret ? '已保存，留空表示不改' : 'sk-…'}"></label></div>
    <div class="companion-key" data-key="env"><label class="field">环境变量名<input name="key_env" value="${escape(config.key_env || '')}" placeholder="DEEPSEEK_API_KEY"></label></div>
    <div class="companion-key" data-key="file"><label class="field">.env 文件位置<input name="key_file" value="${escape(config.key_file || '')}" placeholder="C:\\Users\\你\\companion\\.env"></label><label class="field">变量名<input name="key_name" value="${escape(config.key_name || '')}" placeholder="API_KEY"></label></div>
    <label class="field">模型（可选，留空用服务的第一个）<input name="model" list="companion-models" value="${escape(config.model || '')}" placeholder="deepseek-chat"><datalist id="companion-models"></datalist></label>
    <label class="field">人设（可选，作为 system 消息发送）<textarea name="persona" rows="4" maxlength="8000" placeholder="她是……说话的语气……你们的关系……">${escape(config.persona || '')}</textarea></label>
    <p class="companion-test" data-test-result></p><p class="form-error" role="alert"></p>
    <div class="modal-footer"><button type="button" class="secondary" data-companion-test>测试连接</button><button class="primary">保存</button></div></form>`);
  const form = host.modal.querySelector<HTMLFormElement>('#companion-form')!;
  const source = form.querySelector<HTMLSelectElement>('[name=key_source]')!;
  source.value = config.key_source;
  const preview = form.querySelector<HTMLElement>('[data-avatar-preview]')!;
  const paintAvatar = () => { preview.style.backgroundImage = avatar ? `url('${avatar}')` : ''; preview.textContent = avatar ? '' : (form.querySelector<HTMLInputElement>('[name=name]')!.value.trim()[0] || '伴'); };
  const paintKey = () => form.querySelectorAll<HTMLElement>('[data-key]').forEach(block => { block.hidden = block.dataset.key !== source.value; });
  paintAvatar(); paintKey();
  source.addEventListener('change', paintKey);
  form.querySelector('[name=name]')!.addEventListener('input', paintAvatar);
  form.querySelector<HTMLInputElement>('[data-companion-avatar]')!.addEventListener('change', async event => {
    const file = (event.target as HTMLInputElement).files?.[0];
    if (file) { avatar = await shrink(file); paintAvatar(); }
  });
  const input = () => {
    const value = (name: string) => form.querySelector<HTMLInputElement | HTMLTextAreaElement>(`[name=${name}]`)!.value;
    const secret = value('secret');
    return { name: value('name'), subtitle: value('subtitle') || null, endpoint: value('endpoint'), keySource: source.value, keyEnv: value('key_env') || null, keyFile: value('key_file') || null, keyName: value('key_name') || null, secret: source.value === 'inline' ? (secret || null) : '', model: value('model') || null, persona: value('persona') || null, avatar: avatar || '' };
  };
  form.querySelector('[data-companion-test]')!.addEventListener('click', async () => {
    const result = form.querySelector<HTMLElement>('[data-test-result]')!;
    result.className = 'companion-test'; result.textContent = '正在连接…';
    try {
      const models = await invoke<string[]>('test_companion', { config: input() });
      result.classList.add('ok'); result.textContent = `连上了 · ${models.length} 个模型：${models.slice(0, 6).join('、')}${models.length > 6 ? '…' : ''}`;
      form.querySelector('#companion-models')!.innerHTML = models.map(model => `<option value="${escape(model)}">`).join('');
    } catch (error) { result.classList.add('error'); result.textContent = String(error); }
  });
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const button = form.querySelector<HTMLButtonElement>('button.primary')!; button.disabled = true;
    try { await invoke('save_companion', { config: input() }); host.modal.close(); await host.saved(); host.toast('伴侣配置已保存，点「连接」就能聊了'); }
    catch (error) { form.querySelector('.form-error')!.textContent = String(error); button.disabled = false; }
  });
}
