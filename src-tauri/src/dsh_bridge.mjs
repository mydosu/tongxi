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
    // This declares the compatible request field name only; no output budget is set here.
    // DSH adapter and upstream route defaults are runtime behavior outside this bridge config.
    providers[route.id] = { displayName: route.name, api: 'openai-completions',
      baseURL: route.baseURL, apiKeyEnv: key, headers: route.headers, reasoning: 'low',
      compat: { thinkingFormat: 'deepseek', supportsDeveloperRole: false, maxTokensField: 'max_tokens' },
      models: [{ id: route.model, name: `${route.name} · DS v4.1`, contextWindow: 262144,
        reasoningEfforts: { off: null, low: 'low', high: 'high', max: 'max' } }],
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
  // Keep DSH's native coding tools instead of composing an ACP-only chat shell.
  // These are the base CLI tools with the Windows PowerShell executor; Hermes still
  // supplies the selected model route and credentials to this owned child process.
  process.env.DSH_PERMISSION_MODE = 'danger-full-access';
  const plugins = [
    ['llm', 'dsh-llm'], ['session', 'dsh-session'], ['agent', 'dsh-agent'],
    ['jobs-local', 'dsh-jobs-local'], ['tools', 'dsh-tools'], ['system-prompt', 'dsh-system-prompt'],
    ['session-persistence-jsonl', 'dsh-session-persistence-jsonl'], ['session-projection', 'dsh-session-projection'],
    ['llm-pi-ai', 'dsh-llm-pi-ai'], ['agent-loop', 'dsh-agent-loop'],
    ['sandbox', 'dsh-sandbox-local'], ['sandbox-policy', 'dsh-sandbox-policy'],
    ['subprocess', 'dsh-subprocess-local'], ['pwsh-sandbox', 'dsh-pwsh-sandbox'],
    ['approval', 'dsh-user-approval'], ['permission', 'dsh-permission-presets'],
    ['shell-env', 'dsh-shell-env'], ['tool-pwsh', 'dsh-tool-pwsh'],
    ['tool-jobs', 'dsh-tool-jobs'], ['fs-observation-policy', 'dsh-fs-observation-policy'],
    ['tool-fs', 'dsh-tool-fs'], ['tool-fs-search', 'dsh-tool-fs-search'], ['fs-sandbox', 'dsh-fs-sandbox'],
    ['subagent', 'dsh-subagent'], ['subagent-spawn-in-process', 'dsh-subagent-spawn-in-process'],
    ['subagent-fork-in-process', 'dsh-subagent-fork-in-process'],
    ['tool-subagent-control', 'dsh-tool-subagent-control'],
    ['tool-subagent-list-agents', 'dsh-tool-subagent-control/list-agents'],
    ['tool-subagent', 'dsh-tool-subagent'], ['tool-subagent-fork', 'dsh-tool-subagent'],
    ['tool-todo', 'dsh-tool-todo'], ['goal', 'dsh-goal'], ['goal-round-driver', 'dsh-goal-round-driver'],
    ['command-goal', 'dsh-command-goal'], ['tool-goal', 'dsh-tool-goal'],
    ['workflow-worker-thread', 'dsh-workflow-worker-thread'], ['tool-workflow', 'dsh-tool-workflow'],
    ['tool-ralph', 'dsh-tool-ralph'], ['tool-str-replace-editor', 'dsh-tool-str-replace-editor'],
    ['skill', 'dsh-skill'], ['skill-filesystem', 'dsh-skill-filesystem'], ['tool-skill', 'dsh-tool-skill'],
    ['commands', 'dsh-commands'], ['agent-instructions', 'dsh-agent-instructions'],
    ['web', 'dsh-web'], ['web-search-deepseek', 'dsh-web-search-deepseek'],
    ['web-fetch-http', 'dsh-web-fetch-http'], ['tool-web', 'dsh-tool-web'],
  ];
  const entries = plugins.map(([id, packageName]) => ({ id, name: requireNative.resolve(`@deepseek-ai/${packageName}`) }));
  entries.find(item => item.id === 'session-persistence-jsonl').config = {
    root: join(dshHome, 'sessions'), compression: sharedHome ? 'zstd' : 'none' };
  entries.find(item => item.id === 'system-prompt').config = {
    persona: projectMode
      ? '你是同席项目群中的实现 Agent DSH，按规划承担分配的任务。使用你在 DSH CLI 中配置的工具处理工作，也可调用 hub_message 直接联系群内其他项目 Agent，并用 hub_inbox 读取发给你的消息；如果验收成员不同，完成前须给验收者发简短交接。验收 Agent 会独立复核项目结果。'
      : '你是同席中独立的 DSH 私聊成员，负责简单编码建议和小问题分析。像在 DSH CLI 中一样，按需使用可用工具解决用户任务；直接回答用户，不引用其他会话内容。'
  };
  entries.find(item => item.id === 'sandbox-policy').config = {
    mode: 'danger-full-access', workspaceRoot: process.cwd()
  };
  entries.find(item => item.id === 'approval').config = { policy: 'never' };
  entries.find(item => item.id === 'permission').config = { presets: {
    'read-only': { sandbox: 'read-only', approval: 'ask' },
    'workspace-write': { sandbox: 'workspace-write', approval: 'ask' },
    'danger-full-access': { sandbox: 'danger-full-access', approval: 'never' }
  }};
  entries.find(item => item.id === 'tool-fs-search').config = { sampleOverCapGlobResults: false };
  entries.find(item => item.id === 'tool-subagent').config = {
    provider: 'spawn', toolName: 'subagent', backgroundMode: 'continuable'
  };
  entries.find(item => item.id === 'tool-subagent-fork').config = {
    provider: 'fork', toolName: 'subagent_fork', backgroundMode: 'one-shot'
  };
  entries.find(item => item.id === 'tool-todo').config = { allowParallelInProgress: true };
  entries.find(item => item.id === 'tool-ralph').config = { subagentProvider: 'spawn', maxRounds: 64 };
  entries.find(item => item.id === 'tool-str-replace-editor').config = { maxOutputChars: 16000 };
  entries.find(item => item.id === 'agent-instructions').config = { maxBytes: 65536 };
  entries.find(item => item.id === 'web').config = { searchProvider: 'deepseek-official', fetchProvider: 'http' };
  entries.find(item => item.id === 'web-search-deepseek').config = { apiKeyEnv: 'DEEPSEEK_API_KEY' };
  entries.find(item => item.id === 'tool-web').config = { fetch: true, searchTimeoutMs: 60000 };
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
  // Keep DSH's native tool/plugin surface. Project sessions additionally mount
  // the Agent Hub MCP server for direct messages between group members.
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
