// DSH 桥接回归：用和「更新前检查」相同的方式拉起 dsh_bridge.mjs，逐个模式走 initialize + session/new。
// 不发 prompt，不调模型（设 PROMPT 才真跑一轮）。DSH 改版前后各跑一遍，就知道新版会不会把成员弄坏。
//
//   node scripts/verify_dsh_bridge.mjs <DSH 包目录>...   指定安装（node_modules/@deepseek-ai/dsh）
//   node scripts/verify_dsh_bridge.mjs --latest          另装一份 npm 上的最新版到临时目录再测
import { spawn, execFileSync } from 'node:child_process';
import { readFileSync, mkdtempSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const src = join(dirname(fileURLToPath(import.meta.url)), '../src-tauri/src');
const bridge = readFileSync(join(src, 'dsh_bridge.mjs'), 'utf8');
const MODES = [
  ['私聊', {}],
  ['项目', { AGENT_HUB_DSH_PROJECT_MODE: '1' }],
  ['调度', { AGENT_HUB_COORDINATOR_MODE: '1' }],
];

function installLatest() {
  const prefix = mkdtempSync(join(tmpdir(), 'agenthub-dsh-latest-'));
  execFileSync('npm.cmd', ['install', '--prefix', prefix, '--no-audit', '--no-fund', '@deepseek-ai/dsh@latest'],
    { stdio: 'ignore', shell: true });
  return join(prefix, 'node_modules/@deepseek-ai/dsh');
}

function probe(installation, extraEnv) {
  return new Promise(done => {
    const data = mkdtempSync(join(tmpdir(), 'agenthub-dsh-probe-'));
    const child = spawn(process.execPath, ['--input-type=module', '-e', bridge, installation, data], {
      cwd: installation, stdio: ['pipe', 'pipe', 'ignore'],
      env: { ...process.env, ...extraEnv, AGENT_HUB_DSH_DATA_DIR: data },
    });
    const log = () => ['bridge-error.log', 'bridge-notes.log']
      .map(name => { try { return readFileSync(join(data, name), 'utf8').trim(); } catch { return ''; } }).filter(Boolean).join('\n');
    const finish = (ok, detail) => { clearTimeout(timer); child.removeAllListeners('exit'); child.kill(); done({ ok, detail, log: log() }); };
    const timer = setTimeout(() => finish(false, '超时'), 240000);
    child.on('exit', code => finish(false, `桥退出 ${code}`));
    let buffer = '', id = 0;
    const waiting = new Map();
    child.stdout.on('data', chunk => {
      buffer += chunk;
      let index;
      while ((index = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, index);
        buffer = buffer.slice(index + 1);
        try {
          const message = JSON.parse(line);
          if (message.id && waiting.has(message.id)) { waiting.get(message.id)(message); waiting.delete(message.id); }
          const update = message.params?.update;
          if (update?.sessionUpdate === 'tool_call') process.stdout.write(`    工具 ${update.title}\n`);
        } catch {}
      }
    });
    const rpc = (method, params) => new Promise(resolve => {
      const n = ++id;
      waiting.set(n, resolve);
      child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: n, method, params }) + '\n');
    });
    (async () => {
      const init = await rpc('initialize', { protocolVersion: 1,
        clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false } });
      if (init.error) return finish(false, `initialize：${init.error.message}`);
      const session = await rpc('session/new', { cwd: installation, mcpServers: [] });
      if (session.error) return finish(false, `session/new：${session.error.data?.details || session.error.message}`);
      const model = session.result.configOptions?.find(option => option.id === 'model');
      if (!model) return finish(false, '没有模型选项');
      if (process.env.PROMPT) {
        const reply = await rpc('session/prompt', { sessionId: session.result.sessionId,
          prompt: [{ type: 'text', text: process.env.PROMPT }] });
        if (reply.error) return finish(false, `prompt：${reply.error.message}`);
      }
      finish(true, `模型 ${model.currentValue}`);
    })();
  });
}

const args = process.argv.slice(2);
const installations = args.map(arg => arg === '--latest' ? installLatest() : arg);
if (!installations.length) {
  console.error('用法：node scripts/verify_dsh_bridge.mjs <DSH 包目录>... | --latest');
  process.exit(2);
}
let failed = 0;
for (const installation of installations) {
  if (!existsSync(join(installation, 'package.json'))) { console.log(`✗ ${installation} 不是 DSH 包目录`); failed++; continue; }
  const { version } = JSON.parse(readFileSync(join(installation, 'package.json'), 'utf8'));
  console.log(`DSH ${version}  ${installation}`);
  for (const [label, env] of MODES) {
    const result = await probe(installation, env);
    if (!result.ok) failed++;
    console.log(`  ${result.ok ? '✓' : '✗'} ${label}：${result.detail}`);
    if (result.log) console.log(result.log.split('\n').slice(0, 3).map(line => `    ${line}`).join('\n'));
  }
}
process.exit(failed ? 1 : 0);
