// ACP over stdio for the installed Claude Code CLI. Each prompt runs one native
// `claude -p` process in stream-json mode and resumes the same native session;
// sessions stay in the user's own ~/.claude store, so the CLI can open them too.
import { spawn, execFile } from 'node:child_process';
import { createInterface } from 'node:readline';
import { randomUUID } from 'node:crypto';
import { existsSync, readdirSync, statSync, openSync, readSync, closeSync, readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { homedir } from 'node:os';

// 同席以 `node -e` 内嵌启动（参数从 argv[1] 起）；单独调试时也可 `node claude_bridge.mjs`。
const [exe, data] = process.argv.slice(process.argv[1]?.endsWith('.mjs') ? 2 : 1);
// Windows 上 PowerShell 往原生程序的管道里送文本会补 CRLF，`… | wsl bash -s` 的末行只剩回车符，
// bash 报 command not found 并以 127 退出（前面的命令其实都跑完了）。提前告诉成员换种写法。
const windowsHint = process.platform === 'win32'
  ? '运行环境提示：你在 Windows 上运行。PowerShell 经管道把文本送给原生程序（如 `Get-Content x.sh | wsl -d Ubuntu -- bash -s`）时会补 CRLF，bash 会因末行的回车符报 command not found 并以 127 退出。要在 WSL 里执行脚本，请把脚本存成 LF 换行后按路径运行（`wsl -d Ubuntu -- bash /mnt/d/路径/x.sh`），或用 `wsl -d Ubuntu -- bash -c` 直接传命令，不要用管道喂给 `bash -s`。'
  : '';
// 新会话自带的英文附件会把模型带成英文叙述，任务说明里的中文要求压不住：放进系统提示。
const languageHint = '语言要求：过程说明、回复和交付摘要一律用中文写，代码、命令、路径和英文术语照原样；用户明确要求别的语言时照用户的。';
const memberHint = [languageHint, windowsHint].filter(Boolean).join('\n\n');
const configRoot = process.env.CLAUDE_CONFIG_DIR || join(homedir(), '.claude');
const sessions = new Map();
let catalog = null;
let version = null;

mkdirSync(data, { recursive: true });
// 应用丢弃子进程 stderr；起不来时把原因落到自己的数据目录里，方便排障。
const noteFailure = error => {
  try {
    writeFileSync(join(data, 'bridge-error.log'),
      `${new Date().toISOString()} ${(error && (error.stack || error.message)) || error}\n`, { flag: 'a' });
  } catch {}
};
process.on('unhandledRejection', noteFailure);
process.on('uncaughtException', error => { noteFailure(error); process.exit(1); });

const send = value => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\n');
const update = (sessionId, body) => send({ method: 'session/update', params: { sessionId, update: body } });

// 子进程只继承必要环境；去掉宿主 Claude Code 会话的标记，避免被当成嵌套会话。
function childEnv() {
  const env = { ...process.env };
  for (const key of Object.keys(env)) {
    if (key === 'CLAUDECODE' || key.startsWith('CLAUDE_CODE_SSE') || key.startsWith('AGENT_HUB_')) delete env[key];
  }
  return env;
}

function readVersion() {
  return new Promise(resolve => execFile(exe, ['--version'], { windowsHide: true, timeout: 30000, env: childEnv() },
    (error, stdout) => resolve(error ? null : String(stdout).trim().split(/\s+/)[0] || null)));
}

// 只发 control initialize、不发用户消息：读取本机账户可用的模型和各自的强度等级，不消耗额度。
function readCatalog() {
  return new Promise((resolve, reject) => {
    const child = spawn(exe, ['-p', '--input-format', 'stream-json', '--output-format', 'stream-json', '--verbose'],
      { cwd: data, stdio: ['pipe', 'pipe', 'ignore'], windowsHide: true, env: childEnv() });
    const timer = setTimeout(() => { child.kill(); reject(new Error('Claude 模型目录读取超时')); }, 60000);
    const lines = createInterface({ input: child.stdout });
    lines.on('line', line => {
      let message;
      try { message = JSON.parse(line); } catch { return; }
      if (message.type !== 'control_response' || message.response?.request_id !== 'catalog') return;
      clearTimeout(timer);
      const models = message.response.response?.models;
      child.stdin.end();
      child.kill();
      if (!Array.isArray(models) || !models.length) reject(new Error('Claude 未返回模型目录'));
      else resolve(models.map(model => ({
        value: String(model.value),
        // 「Default (recommended)」本身不带型号，从描述里补上（如「Opus 5.5」），调度才知道它是哪个模型。
        name: withVersion(model.displayName || String(model.value), model.description),
        efforts: model.supportsEffort && Array.isArray(model.supportedEffortLevels) ? model.supportedEffortLevels.map(String) : [],
      })));
    });
    child.on('error', error => { clearTimeout(timer); reject(error); });
    child.stdin.write(JSON.stringify({ type: 'control_request', request_id: 'catalog', request: { subtype: 'initialize' } }) + '\n');
  });
}

// Claude Code 走本地网关时（settings.json 的 env 设了 ANTHROPIC_BASE_URL），网关的 /v1/models 另给真实型号和强度等级；
// CLI 自己的目录只给 sonnet、opus 这类别名，别名还可能被网关映射成别的型号。两份合起来用，见 mergeCatalogs。
function gateway() {
  let settings = {};
  try { settings = JSON.parse(readFileSync(join(configRoot, 'settings.json'), 'utf8')) || {}; } catch {}
  const env = settings.env || {};
  const base = process.env.ANTHROPIC_BASE_URL || env.ANTHROPIC_BASE_URL;
  const token = process.env.ANTHROPIC_AUTH_TOKEN || env.ANTHROPIC_AUTH_TOKEN || process.env.ANTHROPIC_API_KEY || env.ANTHROPIC_API_KEY;
  return base ? { base: String(base).replace(/\/+$/, ''), token, model: settings.model, env: { ...env, ...process.env } } : null;
}

async function readGatewayCatalog() {
  const route = gateway();
  if (!route) return null;
  const response = await fetch(`${route.base}/v1/models`, {
    headers: route.token ? { Authorization: `Bearer ${route.token}` } : {}, signal: AbortSignal.timeout(15000) });
  if (!response.ok) return null;
  const listed = (await response.json())?.data;
  const models = (Array.isArray(listed) ? listed : []).filter(model => model.owned_by === 'claude' || String(model.id).startsWith('claude'));
  if (!models.length) return null;
  const entries = models.map(model => ({ value: String(model.id), name: String(model.display_name || model.id),
    efforts: Array.isArray(model.reasoning_levels) ? model.reasoning_levels.map(String) : [] }));
  // 不指定模型时用 Claude Code 设置里的那个；写明是哪个型号，调度才知道「默认」是什么。
  const preset = entries.find(entry => entry.value === String(route.model || '').replace(/\[1m\]$/i, ''));
  return [{ value: 'default', name: preset ? `默认 · ${preset.name}` : '默认', efforts: preset?.efforts || [] }, ...entries];
}

// CLI 目录和网关目录合起来：已经存下的别名（opus、default 等）照常能选，网关的真实型号也都在；哪份读不到就只用另一份。
// 别名被网关映射成别的型号时（如 sonnet 实际跑 Opus），名字里写明实际跑的是哪个，调度才不会选错。
function mergeCatalogs(cli, routed) {
  if (!cli?.length) return routed;
  if (!routed?.length) return cli;
  const env = gateway()?.env || {};
  const real = new Map(routed.map(entry => [entry.value, entry]));
  const aliases = cli.map(entry => {
    const target = entry.value === 'default' ? routed.find(item => item.value === 'default')
      : real.get(String(env[`ANTHROPIC_DEFAULT_${entry.value.toUpperCase()}_MODEL`] || '').replace(/\[1m\]$/i, ''));
    const actual = target && (entry.value === 'default' ? target.name.replace(/^默认 · /, '') : target.name);
    return actual && !entry.name.includes(actual.replace(/^Claude /, '')) ? { ...entry, name: `${entry.name} → ${actual}` } : entry;
  });
  const known = new Set(aliases.map(entry => entry.value));
  return [...aliases, ...routed.filter(entry => !known.has(entry.value))];
}

function withVersion(name, description) {
  if (/\d/.test(name)) return name;
  const version = String(description || '').split('·')[0].trim();
  return /^[A-Za-z]+ \d+(\.\d+)?$/.test(version) ? `${name} · ${version}` : name;
}

function configOptions(session) {
  const model = catalog.find(entry => entry.value === session.model) || catalog[0];
  return [
    { id: 'model', name: '模型', type: 'select', currentValue: model.value,
      options: catalog.map(entry => ({ value: entry.value, name: entry.name })) },
    { id: 'reasoning_effort', name: '思考强度', type: 'select', currentValue: session.effort || '',
      options: [{ value: '', name: '默认' }, ...model.efforts.map(value => ({ value, name: value }))] },
  ];
}

// 原生会话文件：~/.claude/projects/<cwd 编码>/<id>.jsonl。按 id 找，不依赖目录编码规则。
function transcript(id) {
  const root = join(configRoot, 'projects');
  if (!/^[0-9a-f-]{36}$/i.test(id) || !existsSync(root)) return null;
  for (const folder of readdirSync(root)) {
    const file = join(root, folder, `${id}.jsonl`);
    if (existsSync(file)) return file;
  }
  return null;
}

function head(file, bytes = 65536) {
  const handle = openSync(file, 'r');
  try {
    const buffer = Buffer.alloc(bytes);
    return buffer.subarray(0, readSync(handle, buffer, 0, bytes, 0)).toString('utf8');
  } finally { closeSync(handle); }
}

function tail(file, bytes = 262144) {
  const size = statSync(file).size;
  const handle = openSync(file, 'r');
  try {
    const length = Math.min(bytes, size);
    const buffer = Buffer.alloc(length);
    return buffer.subarray(0, readSync(handle, buffer, 0, length, size - length)).toString('utf8');
  } finally { closeSync(handle); }
}

/// 会话名：用户改过的名字（custom-title）优先，其次 Claude 自动起的标题（ai-title），各取最后一条。
/// 这两类记录常在文件中后段，所以开头和末尾都扫。
function namedTitle(file) {
  let custom = null, ai = null;
  for (const chunk of [head(file), tail(file)]) {
    for (const line of chunk.split('\n')) {
      if (!line.includes('title')) continue;
      let row;
      try { row = JSON.parse(line); } catch { continue; }
      if (row.type === 'custom-title' && row.customTitle) custom = String(row.customTitle);
      if (row.type === 'ai-title' && row.aiTitle) ai = String(row.aiTitle);
    }
  }
  return custom || ai;
}

function listSessions() {
  const root = join(configRoot, 'projects');
  if (!existsSync(root)) return [];
  const files = [];
  for (const folder of readdirSync(root)) {
    const directory = join(root, folder);
    let entries;
    try { entries = readdirSync(directory); } catch { continue; }
    for (const name of entries) {
      if (!/^[0-9a-f-]{36}\.jsonl$/i.test(name)) continue;
      const file = join(directory, name);
      files.push({ file, id: name.slice(0, -6), mtime: statSync(file).mtimeMs });
    }
  }
  files.sort((a, b) => b.mtime - a.mtime);
  return files.slice(0, 100).map(({ file, id, mtime }) => {
    let cwd = null, title = namedTitle(file);
    for (const line of head(file).split('\n')) {
      let row;
      try { row = JSON.parse(line); } catch { continue; }
      if (row.type === 'summary' && row.summary && !title) title = String(row.summary);
      if (row.type !== 'user' || row.isMeta || row.isSidechain) continue;
      cwd ??= row.cwd || null;
      const content = row.message?.content;
      const text = typeof content === 'string' ? content
        : Array.isArray(content) ? content.filter(block => block.type === 'text').map(block => block.text).join(' ') : '';
      if (!title && text.trim() && !text.trimStart().startsWith('<')) title = text.trim().slice(0, 80);
      if (cwd && title) break;
    }
    return { sessionId: id, cwd, title, updatedAt: new Date(mtime).toISOString() };
  });
}

// ACP 的 MCP 描述 → Claude --mcp-config。只接 stdio 形式，与同席注入的项目工具一致。
function mcpConfig(servers) {
  const mcpServers = {};
  for (const server of servers || []) {
    if (!server?.name || !server.command) continue;
    const env = {};
    for (const item of server.env || []) if (item?.name) env[item.name] = String(item.value ?? '');
    mcpServers[server.name] = { type: 'stdio', command: String(server.command), args: (server.args || []).map(String), env };
  }
  return Object.keys(mcpServers).length ? JSON.stringify({ mcpServers }) : null;
}

function claudeContent(prompt) {
  const content = [];
  for (const block of prompt || []) {
    if (block.type === 'text' && block.text) content.push({ type: 'text', text: block.text });
    else if (block.type === 'image' && block.data) {
      content.push({ type: 'image', source: { type: 'base64', media_type: block.mimeType || 'image/png', data: block.data } });
    } else if (block.type === 'resource_link' && block.uri) content.push({ type: 'text', text: `附件：${block.name || ''} ${block.uri}` });
    else if (block.type === 'resource' && block.resource?.text) content.push({ type: 'text', text: block.resource.text });
  }
  if (!content.length) content.push({ type: 'text', text: '' });
  return content;
}

function toolTitle(name, input) {
  const detail = input?.description || input?.command || input?.file_path || input?.pattern || input?.url || input?.query || '';
  return detail ? `${name} · ${String(detail).slice(0, 120)}` : name;
}

function toolKind(name) {
  if (/^(Read|Glob|Grep|LS|NotebookRead)$/.test(name)) return 'read';
  if (/^(Edit|Write|MultiEdit|NotebookEdit)$/.test(name)) return 'edit';
  if (/^(Bash|PowerShell|BashOutput|KillShell)$/.test(name)) return 'execute';
  if (/^(WebFetch|WebSearch)$/.test(name)) return 'fetch';
  if (/^(Task|Agent)$/.test(name)) return 'think';
  return 'other';
}

function resultText(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) return content.map(block => block.type === 'text' ? block.text : `[${block.type}]`).join('\n');
  return content == null ? '' : JSON.stringify(content);
}

// 回合用量按 ACP PromptResponse.usage 回报：inputTokens 含缓存读写，费用放 _meta。
// modelUsage 按模型分列、连子 agent（Task）用的模型一起算，有就用它的合计（子 agent 的消耗记在这位成员身上）；
// 没有再退回主线程的 usage。
// --resume 续接的会话，claude 在结果里报的 modelUsage 和费用是整个会话累计的，不是这一回合的：
// 同一会话接着做下一项任务时，前面任务的用量会再记一遍，越往后越多。记下每个会话上回合的累计值，回报差额。
const baselineFile = join(data, 'usage-baseline.json');
function readBaselines() {
  try { return JSON.parse(readFileSync(baselineFile, 'utf8')) || {}; } catch { return {}; }
}

function usageOf(result, context, sessionId, resumed, spent) {
  const u = result.usage || {};
  const models = Object.values(result.modelUsage || {});
  const sum = key => models.reduce((total, model) => total + (Number(model[key]) || 0), 0);
  const totals = models.length
    ? [sum('cacheReadInputTokens'), sum('cacheCreationInputTokens'), sum('inputTokens'), sum('outputTokens'), sum('thinkingTokens')]
    : [u.cache_read_input_tokens || 0, u.cache_creation_input_tokens || 0, u.input_tokens || 0, u.output_tokens || 0, u.output_tokens_details?.thinking_tokens || 0];
  const cost = Number.isFinite(result.total_cost_usd) ? result.total_cost_usd : null;
  const baselines = readBaselines();
  const base = baselines[sessionId];
  baselines[sessionId] = { totals, cost };
  try { writeFileSync(baselineFile, JSON.stringify(baselines)); } catch {}
  let [read, write, fresh, output, thought] = totals;
  let costUsd = cost;
  if (base && totals.every((value, index) => value >= (base.totals[index] || 0))) {
    [read, write, fresh, output, thought] = totals.map((value, index) => value - (base.totals[index] || 0));
    costUsd = cost !== null && Number.isFinite(base.cost) ? Math.max(0, cost - base.cost) : null;
  } else if (resumed && !base && spent) {
    // 续接的旧会话没有记过累计值：这一回合只能按流里逐条累加的输入估，输出和费用不报。
    return { usage: { inputTokens: spent, outputTokens: 0, totalTokens: spent }, _meta: { contextTokens: context } };
  }
  const input = fresh + read + write;
  if (!input && !output) return {};
  return { usage: { inputTokens: input, outputTokens: output, cachedReadTokens: read, cachedWriteTokens: write, ...(thought ? { thoughtTokens: thought } : {}), totalTokens: input + output },
    _meta: { contextTokens: context, ...(costUsd !== null ? { costUsd } : {}) } };
}

function failureMessage(result) {
  const text = String(result.result || '').toLowerCase();
  if (/usage limit|rate limit|429|limit reached|overloaded/.test(text) || result.api_error_status === 429) {
    return 'Claude 返回额度或频率限制（HTTP 429 rate limit exceeded）';
  }
  if (/auth|login|401|oauth|credential/.test(text)) return 'Claude 认证失败，请在终端运行一次 claude 重新登录';
  if (/prompt is too long|context/.test(text)) return 'Claude 上下文超限';
  return `Claude 未正常完成（${result.subtype || 'error'}）`;
}

function prompt(id, params) {
  const session = sessions.get(params.sessionId);
  if (!session) throw new Error('Claude 会话未加载');
  if (session.child) throw new Error('Claude 正在处理上一条消息');
  const args = ['-p', '--input-format', 'stream-json', '--output-format', 'stream-json', '--verbose',
    '--include-partial-messages', '--disallowedTools', 'AskUserQuestion'];
  if (session.model && session.model !== 'default') args.push('--model', session.model);
  if (session.effort) args.push('--effort', session.effort);
  const resumed = Boolean(session.started || transcript(params.sessionId));
  args.push(resumed ? '--resume' : '--session-id', params.sessionId);
  args.push('--permission-mode', 'bypassPermissions');
  args.push('--append-system-prompt', memberHint);
  const mcp = mcpConfig(session.mcpServers);
  if (mcp) args.push('--mcp-config', mcp);
  const child = spawn(exe, args, { cwd: session.cwd, stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true, env: childEnv() });
  session.child = child;
  session.cancelled = false;
  let stderr = '';
  let result = null;
  let wroteText = false;
  // 最近一次 rate_limit_event：被限流时带上重置时间（Unix 秒），同席据此等到重置再继续。
  let resetsAt = null;
  const blocks = new Map();
  // 最后一次模型调用的输入+输出＝会话当前的上下文大小。
  let context = 0;
  // 本回合到现在的累计用量（每次模型调用的输入含缓存读写＋输出），回合中途报给同席看软预算；按消息 id 去重。
  let spent = 0;
  const counted = new Set();
  child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-4000); });
  const lines = createInterface({ input: child.stdout });
  lines.on('line', line => {
    let message;
    try { message = JSON.parse(line); } catch { return; }
    if (message.session_id && message.session_id === params.sessionId) session.started = true;
    // 子 Agent 的内部输出只在其工具卡片里体现，不混进主回复。
    if (message.parent_tool_use_id) return;
    if (message.type === 'rate_limit_event') {
      const info = message.rate_limit_info || {};
      if (info.status && info.status !== 'allowed' && Number.isFinite(info.resetsAt)) resetsAt = info.resetsAt;
      return;
    }
    if (message.type === 'stream_event') {
      const event = message.event || {};
      if (event.type === 'content_block_start') {
        blocks.set(event.index, event.content_block?.type);
        if (event.content_block?.type === 'text' && wroteText) {
          update(params.sessionId, { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: '\n\n' } });
        }
      } else if (event.type === 'content_block_delta') {
        if (event.delta?.type === 'text_delta' && event.delta.text) {
          wroteText = true;
          update(params.sessionId, { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: event.delta.text } });
        } else if (event.delta?.type === 'thinking_delta' && event.delta.thinking) {
          update(params.sessionId, { sessionUpdate: 'agent_thought_chunk', content: { type: 'text', text: event.delta.thinking } });
        }
      }
    } else if (message.type === 'assistant') {
      const used = message.message?.usage;
      if (used) context = (used.input_tokens || 0) + (used.cache_read_input_tokens || 0) + (used.cache_creation_input_tokens || 0) + (used.output_tokens || 0);
      const messageId = message.message?.id;
      if (used && messageId && !counted.has(messageId)) {
        counted.add(messageId);
        spent += context;
        update(params.sessionId, { sessionUpdate: 'usage_update', used: context, spent });
      }
      for (const block of message.message?.content || []) {
        if (block.type !== 'tool_use') continue;
        update(params.sessionId, { sessionUpdate: 'tool_call', toolCallId: block.id, title: toolTitle(block.name, block.input),
          kind: toolKind(block.name), status: 'in_progress', rawInput: block.input });
      }
    } else if (message.type === 'user') {
      const content = message.message?.content;
      for (const block of Array.isArray(content) ? content : []) {
        if (block.type !== 'tool_result') continue;
        update(params.sessionId, { sessionUpdate: 'tool_call_update', toolCallId: block.tool_use_id,
          status: block.is_error ? 'failed' : 'completed',
          content: [{ type: 'content', content: { type: 'text', text: resultText(block.content).slice(0, 8000) } }] });
      }
    } else if (message.type === 'result') {
      // 续会话时上一单没跑完的后台命令会先来一条通知，claude 为它出一个零轮的空结果，真正这一单还在后面。
      if (message.num_turns === 0 && !message.is_error) return;
      result = message;
      child.stdin.end();
      // 回合已经结束，但 claude 偶尔迟迟不退出（子 agent 或 MCP 子进程还挂着）；再等 20 秒还不退就结束它，免得协作一直等下去。
      session.exitTimer = setTimeout(() => {
        if (child.exitCode === null) execFile('taskkill', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true }, () => {});
      }, 20000);
    }
  });
  child.on('error', error => {
    session.child = null;
    send({ id, error: { code: -32603, message: `Claude 启动失败：${error.message}` } });
  });
  child.on('exit', code => {
    session.child = null;
    clearTimeout(session.killTimer);
    clearTimeout(session.exitTimer);
    if (session.cancelled || result?.terminal_reason?.startsWith('aborted')) {
      send({ id, result: { stopReason: 'cancelled' } });
    } else if (result && !result.is_error) {
      const reason = result.stop_reason === 'max_tokens' ? 'max_tokens'
        : result.stop_reason === 'refusal' ? 'refusal' : 'end_turn';
      send({ id, result: { stopReason: reason, ...usageOf(result, context, params.sessionId, resumed, spent) } });
    } else if (result) {
      noteFailure(`prompt failed: ${result.subtype} ${String(result.result || '').slice(0, 300)}`);
      const limited = /429|额度/.test(failureMessage(result));
      // 没收到限流事件时，从结果文字里只摘出“resets 5:50pm”这类时间短语，不转发原文。
      const phrase = String(result.result || '').match(/(?:resets?|try again)\s+(?:at\s+|in\s+)?[0-9][0-9a-z: ]{0,30}/i)?.[0]?.trim();
      const stamp = Number(String(result.result || '').match(/limit reached\|(\d{10})\b/i)?.[1]) || null;
      const data = (resetsAt || stamp) ? { resets_at: resetsAt || stamp } : phrase ? { reset_hint: phrase.slice(0, 40) } : null;
      send({ id, error: { code: -32603, message: failureMessage(result), ...(limited && data ? { data } : {}) } });
    } else {
      noteFailure(`claude exited ${code}: ${stderr.slice(-1000)}`);
      send({ id, error: { code: -32603, message: `Claude 进程意外退出（${code}）` } });
    }
  });
  child.stdin.write(JSON.stringify({ type: 'user', message: { role: 'user', content: claudeContent(params.prompt) } }) + '\n');
}

// 回复进行中插话：把引导作为新的用户消息写进运行中进程的 stdin，Claude 在下一步原生采纳。
function steer(params) {
  const session = sessions.get(params?.sessionId);
  if (!session?.child || session.cancelled || !session.child.stdin.writable || session.child.stdin.writableEnded) {
    throw new Error('Claude 当前没有进行中的回复');
  }
  session.child.stdin.write(JSON.stringify({ type: 'user', message: { role: 'user', content: claudeContent(params.prompt) } }) + '\n');
  return {};
}

function cancel(params) {
  const session = sessions.get(params?.sessionId);
  if (!session?.child) return;
  session.cancelled = true;
  try {
    session.child.stdin.write(JSON.stringify({ type: 'control_request', request_id: randomUUID(), request: { subtype: 'interrupt' } }) + '\n');
  } catch {}
  const child = session.child;
  session.killTimer = setTimeout(() => {
    if (child.exitCode === null) execFile('taskkill', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true }, () => {});
  }, 8000);
}

function open(params, existing) {
  if (!params?.cwd) throw new Error('缺少会话目录');
  const sessionId = existing ? params.sessionId : randomUUID();
  if (existing && !/^[0-9a-f-]{36}$/i.test(sessionId || '')) throw new Error('Claude 会话 ID 无效');
  const previous = sessions.get(sessionId);
  const session = { cwd: params.cwd, mcpServers: params.mcpServers || [], model: previous?.model ?? null,
    effort: previous?.effort ?? null, started: existing && Boolean(transcript(sessionId)), child: previous?.child ?? null };
  sessions.set(sessionId, session);
  return { sessionId, configOptions: configOptions(session) };
}

async function handle(message) {
  const { id, method, params } = message;
  switch (method) {
    case 'initialize':
      const [cli, routed] = await Promise.all([readCatalog().catch(() => null), readGatewayCatalog().catch(() => null),
        readVersion().then(value => { version = value; })]);
      catalog = mergeCatalogs(cli, routed);
      if (!catalog?.length) throw new Error('Claude 模型目录读取失败');
      return { protocolVersion: 1, agentInfo: { name: 'claude-code', title: 'Claude Code', version },
        agentCapabilities: { loadSession: true, promptCapabilities: { image: true, embeddedContext: true } }, authMethods: [] };
    case 'session/new':
      return open(params, false);
    case 'session/resume':
    case 'session/load':
      return open(params, true);
    case 'session/list':
      return { sessions: listSessions() };
    case 'session/close':
      sessions.delete(params?.sessionId);
      return {};
    case 'session/set_model':
    case 'session/set_config_option': {
      const session = sessions.get(params?.sessionId);
      if (!session) throw new Error('Claude 会话未加载');
      const configId = method === 'session/set_model' ? 'model' : params.configId;
      const value = method === 'session/set_model' ? params.modelId : params.value;
      if (configId === 'model') {
        const model = catalog.find(entry => entry.value === value);
        if (!model) throw new Error('Claude 模型不可用');
        session.model = model.value;
        if (session.effort && !model.efforts.includes(session.effort)) session.effort = null;
      } else if (configId === 'reasoning_effort') {
        const model = catalog.find(entry => entry.value === session.model) || catalog[0];
        const effort = value === 'default' ? '' : String(value ?? '');
        if (effort && !model.efforts.includes(effort)) throw new Error('该 Claude 模型不支持此思考强度');
        session.effort = effort || null;
      } else throw new Error('不支持的配置项');
      return { configOptions: configOptions(session) };
    }
    case 'session/prompt':
      prompt(id, params);
      return undefined;
    case 'session/steer':
      return steer(params);
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
  for (const session of sessions.values()) if (session.child) session.child.kill();
  process.exit(0);
});
