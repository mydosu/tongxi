// ACP over stdio for the installed Antigravity CLI (`agy`). Each prompt runs one
// `agy -p=` process in stream-json mode and resumes the same native conversation
// (`--conversation <id>`); conversations stay in the user's own ~/.gemini store.
// agy takes no MCP servers on the command line, but reads `.agents/mcp_config.json` in every --add-dir
// directory: each session gets its own such directory under the bridge's data dir.
import { spawn, execFile } from 'node:child_process';
import { createInterface } from 'node:readline';
import { randomUUID } from 'node:crypto';
import { existsSync, readFileSync, writeFileSync, mkdirSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { homedir } from 'node:os';

// 同席以 `node -e` 内嵌启动（参数从 argv[1] 起）；单独调试时也可 `node antigravity_bridge.mjs`。
const [exe, data] = process.argv.slice(process.argv[1]?.endsWith('.mjs') ? 2 : 1);
const conversations = join(homedir(), '.gemini', 'antigravity-cli', 'conversations');
const sessions = new Map();
let catalog = [];
let version = null;

mkdirSync(data, { recursive: true });
// 上次异常退出留下的工具配置：每回合都会重写，直接清掉。
rmSync(join(data, 'mcp'), { recursive: true, force: true });
const noteFailure = error => {
  try { writeFileSync(join(data, 'bridge-error.log'), `${new Date().toISOString()} ${(error && (error.stack || error.message)) || error}\n`, { flag: 'a' }); } catch {}
};
process.on('unhandledRejection', noteFailure);
process.on('uncaughtException', error => { noteFailure(error); process.exit(1); });

const send = value => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\n');
const update = (sessionId, body) => send({ method: 'session/update', params: { sessionId, update: body } });

// 同席的会话 ID 在第一回合前就要给出，agy 的对话 ID 第一回合才有：两者的对应存在自己的数据目录里。
const linksFile = join(data, 'conversations.json');
const readLinks = () => { try { return JSON.parse(readFileSync(linksFile, 'utf8')) || {}; } catch { return {}; } };
function link(sessionId, conversation) {
  const links = readLinks();
  if (links[sessionId] === conversation) return;
  links[sessionId] = conversation;
  try { writeFileSync(linksFile, JSON.stringify(links)); } catch {}
}
// 同席开的会话查对应表；CLI 里开的对话本身就是 agy 的 ID。
const conversationOf = sessionId => readLinks()[sessionId] || (existsSync(join(conversations, `${sessionId}.db`)) ? sessionId : null);

function run(args) {
  return new Promise((resolve, reject) => execFile(exe, args, { windowsHide: true, timeout: 60000 }, (error, stdout) => error ? reject(error) : resolve(String(stdout))));
}

function configOptions(session) {
  return [
    { id: 'model', name: '模型', type: 'select', currentValue: session.model || catalog[0]?.value || '',
      options: catalog.map(entry => ({ value: entry.value, name: entry.name })) },
    // 只读档切 plan：命令一律不放行；build 自动放行全部工具（无头模式没法弹确认）。
    { id: 'mode', name: '模式', type: 'select', currentValue: session.mode,
      options: [{ value: 'build', name: '执行' }, { value: 'plan', name: '只读' }] },
  ];
}

// 回合结束就删：配置每回合重写，agy 按服务名缓存的工具说明也会在下次启动时重新生成。
function dropMcp(session) {
  for (const path of session.mcpLeftovers || []) rmSync(path, { recursive: true, force: true });
  session.mcpLeftovers = null;
}

// 同席的项目工具（MCP）：写进这个会话自己的目录，用 --add-dir 带上；无头模式要显式放行。
function mcpArgs(session, sessionId, servers) {
  const mcpServers = {};
  for (const server of servers || []) {
    if (!server?.name || !server.command) continue;
    const env = {};
    for (const item of server.env || []) if (item?.name) env[item.name] = String(item.value ?? '');
    mcpServers[server.name] = { command: String(server.command), args: (server.args || []).map(String), env };
  }
  const names = Object.keys(mcpServers);
  if (!names.length) return [];
  const root = join(data, 'mcp', sessionId.replace(/[^0-9a-zA-Z-]/g, ''));
  mkdirSync(join(root, '.agents'), { recursive: true });
  writeFileSync(join(root, '.agents', 'mcp_config.json'), JSON.stringify({ mcpServers }));
  session.mcpLeftovers = [root, ...names.map(name => join(homedir(), '.gemini', 'antigravity-cli', 'mcp', name))];
  return ['--add-dir', root, '--settings', JSON.stringify({ permissions: { allow: names.map(name => `mcp(${name}/*)`) } })];
}

function promptText(prompt) {
  return (prompt || []).map(block => block.type === 'text' ? block.text
    : block.type === 'resource_link' ? `附件：${block.name || ''} ${block.uri}`
    : block.type === 'resource' ? block.resource?.text || '' : '').filter(Boolean).join('\n\n');
}

function toolKind(name) {
  if (/^(view_file|read_file|list_dir|find_by_name|grep_search|read_url_content|read_resource)$/.test(name)) return 'read';
  if (/(replace_file_content|write_to_file|sed_file|notebook_edit)/.test(name)) return 'edit';
  if (/^(run_command|send_command_input|command_status)$/.test(name)) return 'execute';
  if (/^(search_web|open_browser_url|read_browser_page)$/.test(name)) return 'fetch';
  return 'other';
}

function toolTitle(info) {
  const p = info?.parameters || {};
  const detail = p.CommandLine || p.AbsolutePath || p.Path || p.TargetFile || p.Query || p.Url || p.SearchPath || '';
  return detail ? `${info.name} · ${String(detail).slice(0, 120)}` : info?.name || '工具';
}

function usageOf(u) {
  if (!u) return {};
  const input = u.input_tokens || 0, output = u.output_tokens || 0;
  if (!input && !output) return {};
  return { usage: { inputTokens: input, outputTokens: output, cachedReadTokens: u.cache_read_tokens || 0,
    ...(u.thinking_tokens ? { thoughtTokens: u.thinking_tokens } : {}), totalTokens: input + output } };
}

function prompt(id, params) {
  const session = sessions.get(params.sessionId);
  if (!session) throw new Error('Antigravity 会话未加载');
  if (session.child) throw new Error('Antigravity 正在处理上一条消息');
  session.cancelled = false;
  const args = ['--input-format', 'stream-json', '--output-format', 'stream-json'];
  if (session.model) args.push('--model', session.model);
  const conversation = conversationOf(params.sessionId);
  if (conversation) args.push('--conversation', conversation);
  args.push(...mcpArgs(session, params.sessionId, session.mcpServers));
  if (session.mode === 'plan') args.push('--mode', 'plan');
  // shortcut: agy 的 deny 规则只认整条命令、跳过权限时还不生效，写档挡不住 git push / gh release；agy 支持前缀规则后改成 allow 全部 + deny 不可逆命令。
  else args.push('--dangerously-skip-permissions');
  args.push('-p=');
  const child = spawn(exe, args, { cwd: session.cwd, stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
  session.child = child;
  let stderr = '', result = null, wroteText = false;
  const steps = new Set();
  child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-4000); });
  createInterface({ input: child.stdout }).on('line', line => {
    let message;
    try { message = JSON.parse(line); } catch { return; }
    if (message.event === 'init' && message.conversation_id) link(params.sessionId, message.conversation_id);
    else if (message.event === 'step_update') {
      const step = message.step_update || {};
      if (step.step_type === 'agent_response') {
        // 一个回合里多段回复之间空一行，别粘在一起。
        if (step.text_delta && !steps.has(step.step_index) && wroteText) update(params.sessionId, { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: '\n\n' } });
        if (step.thinking_delta) update(params.sessionId, { sessionUpdate: 'agent_thought_chunk', content: { type: 'text', text: step.thinking_delta } });
        if (step.text_delta) { steps.add(step.step_index); wroteText = true; update(params.sessionId, { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: step.text_delta } }); }
      } else if (step.step_type === 'tool') {
        const info = step.tool_info || { name: step.tool_name };
        const toolCallId = `${conversationOf(params.sessionId) || params.sessionId}-${step.step_index}`;
        if (step.state === 'ACTIVE') update(params.sessionId, { sessionUpdate: 'tool_call', toolCallId, title: toolTitle(info), kind: toolKind(info.name), status: 'in_progress', rawInput: info.parameters });
        else {
          const text = info.error?.message || (typeof info.output === 'string' ? info.output : info.output ? JSON.stringify(info.output) : '');
          update(params.sessionId, { sessionUpdate: 'tool_call_update', toolCallId, status: step.state === 'ERROR' ? 'failed' : 'completed',
            ...(text ? { content: [{ type: 'content', content: { type: 'text', text: text.slice(0, 8000) } }] } : {}) });
        }
      }
    } else if (message.event === 'result') result = message.result || {};
  });
  child.on('error', error => {
    if (session.child !== child) return;
    session.child = null;
    dropMcp(session);
    send({ id, error: { code: -32603, message: `Antigravity 启动失败：${error.message}` } });
  });
  child.on('exit', code => {
    if (session.child !== child) return;
    session.child = null;
    clearTimeout(session.killTimer);
    dropMcp(session);
    if (session.cancelled) return send({ id, result: { stopReason: 'cancelled' } });
    if (result?.status === 'SUCCESS') {
      // 无头模式下没被放行的工具会被拒掉，回复可能是空的：说清楚，别当成正常完成。
      if (!String(result.response || '').trim() && !wroteText && result.denied_actions?.length) {
        return send({ id, error: { code: -32603, message: `Antigravity 的操作被拒绝（${result.denied_actions.map(item => item.display_name || item.action).join('、')}）：只读模式下不能执行` } });
      }
      if (!wroteText && result.response) update(params.sessionId, { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: result.response } });
      return send({ id, result: { stopReason: 'end_turn', ...usageOf(result.usage) } });
    }
    const reason = result?.error || stderr.trim().split('\n').pop() || `进程退出（${code}）`;
    noteFailure(`agy exited ${code}: ${reason} ${stderr.slice(-1000)}`);
    send({ id, error: { code: -32603, message: `Antigravity 回复失败：${String(reason).slice(0, 300)}` } });
  });
  child.stdin.end(JSON.stringify({ event: 'user', message: { content: promptText(params.prompt) } }) + '\n');
}

function cancel(params) {
  const session = sessions.get(params?.sessionId);
  if (!session?.child) return;
  session.cancelled = true;
  execFile('taskkill', ['/PID', String(session.child.pid), '/T', '/F'], { windowsHide: true }, () => {});
}

function open(params, existing) {
  if (!params?.cwd) throw new Error('缺少会话目录');
  const sessionId = existing ? params.sessionId : randomUUID();
  if (existing && !conversationOf(sessionId)) throw new Error('Antigravity 对话不存在');
  const session = Object.assign(sessions.get(sessionId) || { model: null, mode: 'build' }, { cwd: params.cwd, mcpServers: params.mcpServers || [] });
  sessions.set(sessionId, session);
  return { sessionId, configOptions: configOptions(session) };
}

async function handle(message) {
  const { method, params } = message;
  switch (method) {
    case 'initialize': {
      const [models, about] = await Promise.all([run(['models']), run(['--version']).catch(() => '')]);
      // `agy models` 每行「id<TAB>名称」。
      catalog = models.split(/\r?\n/).map(line => line.split('\t')).filter(parts => parts.length >= 2 && parts[0].trim())
        .map(([value, name]) => ({ value: value.trim(), name: name.trim() }));
      if (!catalog.length) throw new Error('Antigravity 模型目录读取失败（是否已登录 agy？）');
      version = about.trim().split(/\s+/).pop() || null;
      return { protocolVersion: 1, agentInfo: { name: 'antigravity', title: 'Antigravity', version },
        agentCapabilities: { loadSession: true, promptCapabilities: { image: false, embeddedContext: true } }, authMethods: [] };
    }
    case 'session/new':
      return open(params, false);
    case 'session/resume':
    case 'session/load':
      return open(params, true);
    case 'session/close':
      sessions.delete(params?.sessionId);
      return {};
    case 'session/set_config_option': {
      const session = sessions.get(params?.sessionId);
      if (!session) throw new Error('Antigravity 会话未加载');
      if (params.configId === 'model') {
        if (!catalog.some(entry => entry.value === params.value)) throw new Error('Antigravity 模型不可用');
        session.model = params.value;
      } else if (params.configId === 'mode') {
        if (!['build', 'plan'].includes(params.value)) throw new Error('Antigravity 模式无效');
        session.mode = params.value;
      } else throw new Error('不支持的配置项');
      return { configOptions: configOptions(session) };
    }
    case 'session/prompt':
      prompt(message.id, params);
      return undefined;
    default: {
      const error = new Error(`不支持的方法：${method}`);
      error.code = -32601;
      throw error;
    }
  }
}

createInterface({ input: process.stdin }).on('line', line => {
  let message;
  try { message = JSON.parse(line); } catch { return; }
  if (message.method === 'session/cancel') return cancel(message.params);
  if (message.id === undefined || !message.method) return;
  Promise.resolve().then(() => handle(message)).then(result => {
    if (result !== undefined) send({ id: message.id, result });
  }, error => send({ id: message.id, error: { code: error.code || -32603, message: String(error.message || error) } }));
});
process.stdin.on('end', () => {
  for (const session of sessions.values()) if (session.child) execFile('taskkill', ['/PID', String(session.child.pid), '/T', '/F'], { windowsHide: true }, () => {});
  process.exit(0);
});
