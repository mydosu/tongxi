// Reuse the installed harness; own only the chat composition and session files.
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

const [installation, data] = process.argv.slice(1);
const projectMode = process.env.AGENT_HUB_DSH_PROJECT_MODE === '1';
const requireNative = createRequire(join(installation, 'package.json'));
const load = name => import(pathToFileURL(requireNative.resolve(name)).href);
let ctx;
mkdirSync(data, { recursive: true });
// 应用把子进程 stderr 丢掉了，起不来时把原因落到自己的数据目录里，方便排障。
const noteFailure = error => {
  try {
    writeFileSync(join(data, 'bridge-error.log'),
      `${new Date().toISOString()} ${(error && (error.stack || error.message)) || error}\n`, { flag: 'a' });
  } catch {}
};
process.on('unhandledRejection', noteFailure);
process.on('uncaughtException', error => { noteFailure(error); process.exit(1); });
try {
  const { boot, loadLayeredEnv } = await load('@deepseek-ai/dsh-app-boot');
  const { DSH_LAUNCH_ENVIRONMENT_KEY } = await load('@deepseek-ai/dsh-launch-environment');
  const hermesRepo = process.env.AGENT_HUB_DSH_HERMES_REPO || join(process.env.LOCALAPPDATA, 'hermes/hermes-agent');
  const python = process.env.AGENT_HUB_HERMES_PYTHON || join(hermesRepo, '.venv/Scripts/python.exe');
  const { stdout } = await promisify(execFile)(python, ['-u', '-c', process.env.AGENT_HUB_DSH_AUTH_BRIDGE,
    hermesRepo, '--runtime'], { windowsHide: true, timeout: 45000, maxBuffer: 262144 });
  const routes = JSON.parse(stdout);
  if (routes.length !== 2) throw new Error('Expected the two authorized routes');
  const providers = {};
  routes.forEach((route, index) => {
    const key = `AGENT_HUB_DSH_COMMAND_CODE_${index+1}`;
    process.env[key] = route.apiKey;
    providers[route.id] = { displayName: route.name, api: 'openai-completions',
      baseURL: route.baseURL, apiKeyEnv: key, headers: route.headers, reasoning: 'low',
      compat: { thinkingFormat: 'deepseek', supportsDeveloperRole: false, maxTokensField: 'max_tokens' },
      models: [{ id: route.model, name: `${route.name} · DS v4.1`, contextWindow: 262144,
        maxTokens: 16384, reasoningEfforts: { off: null, low: 'low', high: 'high', max: 'max' } }],
      retryPolicy: { mode: 'normal', maxRetries: 0 } };
    route.apiKey = null;
  });
  // The native launch snapshot retains the private environment in this child.
  const environment = loadLayeredEnv('agent-hub-dsh', data, () => {});
  // 共用她自己的 DSH 家目录（同一个会话库）时用她的库，编码对齐她客户端的 zstd；
  // 不给 AGENT_HUB_DSH_HOME 就回到同席自己的隔离目录（测试默认）。
  const sharedHome = process.env.AGENT_HUB_DSH_HOME ? resolve(process.env.AGENT_HUB_DSH_HOME) : null;
  const dshHome = sharedHome ?? resolve(data);
  process.env.DSH_HOME = dshHome;
  mkdirSync(dshHome, { recursive: true });
  const names = ['llm', 'session', 'agent', 'jobs-local', 'tools', 'system-prompt',
    'session-persistence-jsonl', 'session-projection', 'llm-pi-ai', 'agent-loop'];
  const entries = names.map(name => ({ id: name, name: requireNative.resolve(`@deepseek-ai/dsh-${name}`) }));
  entries.find(item => item.id === 'session-persistence-jsonl').config = {
    root: join(dshHome, 'sessions'), compression: sharedHome ? 'zstd' : 'none' };
  entries.find(item => item.id === 'system-prompt').config = {
    persona: projectMode
      ? '你是同席项目中唯一的代码实施者与实际修复者 DSH，承担明确分配的编码任务并可承担复杂任务。只使用 agent_hub 提供的文件工具，严格遵守当前任务的文件范围和 SHA256 冲突检查。不要读取其他会话、凭据或目录，不运行命令，不接入其他服务。完成后说明真实修改；验收 agent 会独立复核需求和授权源码。'
      : '你是同席中独立的 DSH 私聊成员，负责简单编码建议和小问题分析。本阶段只聊天，没有绑定项目，工具执行关闭。直接回答用户，不读取其他会话或凭据。'
  };
  entries.find(item => item.id === 'agent-loop').config = { agents: [] };
  entries.push({ id: 'acp', name: requireNative.resolve('@deepseek-ai/dsh-acp'), inject: ['agentLoop'], config: {
    provider: routes[0].id, model: routes[0].model
  }});
  const config = join(resolve(data), projectMode ? 'project-composition.json' : 'chat-composition.json');
  writeFileSync(config, JSON.stringify(entries));
  // Internal diagnostics may contain configuration; never forward their text.
  console.log = console.warn = console.error = (...args) => {
    if (process.env.AGENT_HUB_DSH_DIAGNOSTICS !== '1') return;
    const summary = args.map(arg => {
      const text = arg instanceof Error ? arg.stack || '' : String(arg);
      return { errorClass: arg instanceof Error ? arg.name : null,
        frames: [...text.matchAll(/dsh-[a-z0-9-]+[\\/]lib[\\/][a-z0-9./-]+:\d+:\d+/gi)].map(match => match[0]),
        knownTerms: ['undefined', 'not a function', 'apiKey', '401', '402', '404', 'tools', 'model', 'Promise', 'sessionProjections', 'schema', 'getForRequest'].filter(term => text.includes(term)) };
    });
    process.stderr.write(JSON.stringify(summary)+'\n');
  };
  // Route endpoint/headers stay in memory; the disk composition has no secrets.
  ctx = await boot('agent-hub-dsh', config, [{ id: 'llm-pi-ai', config: { providers } }], host => {
    host.provide(DSH_LAUNCH_ENVIRONMENT_KEY, environment);
  }, pathToFileURL(join(installation, 'package.json')).href);
  // No shell/fs/hooks/telemetry plugins. Dedicated project sessions mount only
  // the hub's scoped MCP helper through native ACP; chat sessions remain empty.
  process.stdin.on('end', () => {
    void ctx.fiber.dispose().finally(() => process.exit(0));
  });
  for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => {
    void ctx.fiber.dispose().finally(() => process.exit(0));
  });
} catch (error) {
  if (ctx) await ctx.fiber.dispose().catch(() => {});
  noteFailure(error);
  process.stderr.write('DSH chat bridge startup failed\n');
  process.exit(1);
}
