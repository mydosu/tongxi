// 直接以 ACP 驱动 claude_bridge.mjs：握手与模型目录、新会话工具回合、中断、续接同一原生会话、会话列表。
// 会发送少量真实模型请求（haiku + low），用于 Claude 成员桥接的独立验收；不经过同席界面。
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const exe = process.env.AGENT_HUB_CLAUDE_EXE || 'D:/AI/claude/bin/node_modules/@anthropic-ai/claude-code/bin/claude.exe';
const bridge = join(dirname(fileURLToPath(import.meta.url)), '../src-tauri/src/claude_bridge.mjs');
const work = mkdtempSync(join(tmpdir(), 'tongxi-claude-bridge-'));
writeFileSync(join(work, 'calc.py'), 'def square(value):\n    return value * value\n');

function start() {
  const child = spawn(process.execPath, [bridge, exe, join(work, 'data')], { stdio: ['pipe', 'pipe', 'inherit'] });
  const pending = new Map();
  const updates = [];
  let next = 1;
  createInterface({ input: child.stdout }).on('line', line => {
    const message = JSON.parse(line);
    if (message.method === 'session/update') updates.push(message.params);
    else if (pending.has(message.id)) {
      const { resolve, reject } = pending.get(message.id);
      pending.delete(message.id);
      message.error ? reject(new Error(message.error.message)) : resolve(message.result);
    }
  });
  const rpc = (method, params) => new Promise((resolve, reject) => {
    const id = next++;
    pending.set(id, { resolve, reject });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
  const notify = (method, params) => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method, params }) + '\n');
  return { child, rpc, notify, updates };
}

const checks = [];
const check = (name, ok) => { checks.push([name, Boolean(ok)]); console.log(ok ? 'PASS' : 'FAIL', name); };
const text = (updates, sid, kind = 'agent_message_chunk') =>
  updates.filter(u => u.sessionId === sid && u.update.sessionUpdate === kind).map(u => u.update.content.text).join('');

let a = start();
const init = await a.rpc('initialize', { protocolVersion: 1, clientCapabilities: {} });
check('initialize reports Claude Code version', /^\d+\.\d+\.\d+/.test(init.agentInfo.version || ''));
const created = await a.rpc('session/new', { cwd: work, mcpServers: [] });
const sid = created.sessionId;
const models = created.configOptions.find(o => o.id === 'model').options.map(o => o.value);
check('catalog lists haiku and sonnet', models.includes('haiku') && models.includes('sonnet'));
const set = await a.rpc('session/set_config_option', { sessionId: sid, configId: 'model', value: 'haiku' });
const efforts = set.configOptions.find(o => o.id === 'reasoning_effort').options.map(o => o.value);
check('haiku exposes effort levels', efforts.includes('low'));
await a.rpc('session/set_config_option', { sessionId: sid, configId: 'reasoning_effort', value: 'low' });
let done = await a.rpc('session/prompt', { sessionId: sid, prompt: [{ type: 'text', text: '用 Read 工具读 calc.py，然后只用一句话说出函数名。' }] });
check('tool turn ends normally', done.stopReason === 'end_turn');
check('tool call streamed', a.updates.some(u => u.update.sessionUpdate === 'tool_call'));
check('tool result streamed', a.updates.some(u => u.update.sessionUpdate === 'tool_call_update' && u.update.status === 'completed'));
check('answer mentions square', text(a.updates, sid).includes('square'));

const running = a.rpc('session/prompt', { sessionId: sid, prompt: [{ type: 'text', text: '从1数到300，每个数字一行，不要用工具。' }] });
setTimeout(() => a.notify('session/cancel', { sessionId: sid }), 4000);
done = await running;
check('cancel reports cancelled', done.stopReason === 'cancelled');
a.child.stdin.end();

const b = start();
await b.rpc('initialize', { protocolVersion: 1, clientCapabilities: {} });
await b.rpc('session/resume', { sessionId: sid, cwd: work, mcpServers: [] });
await b.rpc('session/set_config_option', { sessionId: sid, configId: 'model', value: 'haiku' });
done = await b.rpc('session/prompt', { sessionId: sid, prompt: [{ type: 'text', text: '我第一次问你的是哪个文件？只回答文件名。' }] });
check('resumed session remembers earlier turn', done.stopReason === 'end_turn' && text(b.updates, sid).includes('calc.py'));
const listed = await b.rpc('session/list', {});
check('session/list includes the session', listed.sessions.some(s => s.sessionId === sid));
b.child.stdin.end();

const failed = checks.filter(([, ok]) => !ok).length;
console.log(JSON.stringify({ passed: checks.length - failed, failed, work }));
process.exit(failed ? 1 : 0);
