// Reuse the installed harness; own only the chat composition and session files.
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';

const [installation, data] = process.argv.slice(1);
const projectMode = process.env.AGENT_HUB_DSH_PROJECT_MODE === '1';
const coordinatorMode = process.env.AGENT_HUB_COORDINATOR_MODE === '1';
const requireNative = createRequire(join(installation, 'package.json'));
const load = name => import(pathToFileURL(requireNative.resolve(name)).href);
const resolvePackage = name => { try { return requireNative.resolve(`@deepseek-ai/${name}`); } catch { return null; } };
const hasPackage = name => resolvePackage(name) !== null;
// 官方默认组合（dsh-base 的 cordis.patch.yml）里「插件 id → 包名」的对照，以及不带 config 的条目。
// DSH 改包名、拆包时跟着官方走，不靠这里写死的名字；读不到就退回写死的名字。
const officialPlugins = (() => {
  const names = new Map(), bare = new Set();
  try {
    let base;
    try { base = dirname(requireNative.resolve('@deepseek-ai/dsh-base/package.json')); }
    catch { base = join(installation, 'node_modules/@deepseek-ai/dsh-base'); }
    const blocks = readFileSync(join(base, 'cordis.patch.yml'), 'utf8').split(/^\s*- id:\s*/m).slice(1);
    for (const block of blocks) {
      const id = block.match(/^[\w-]+/)?.[0];
      const name = block.match(/^\s+name:\s*'@deepseek-ai\/([^']+)'/m)?.[1];
      if (!id || !name) continue;
      names.set(id, name);
      if (!/^\s+config:/m.test(block.split(/\n\s*\n/)[0])) bare.add(id);
    }
  } catch {}
  return { names, bare };
})();
const skipped = [];
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
  // The native launch snapshot retains the private environment in this child.
  const environment = loadLayeredEnv('agent-hub-dsh', data, () => {});
  // 共用她自己的 DSH 家目录（同一个会话库）时用她的库，编码对齐她客户端的 zstd；
  // 不给 AGENT_HUB_DSH_HOME 就回到同席自己的隔离目录（测试默认）。
  const sharedHome = process.env.AGENT_HUB_DSH_HOME ? resolve(process.env.AGENT_HUB_DSH_HOME) : null;
  const dshHome = sharedHome ?? resolve(data);
  process.env.DSH_HOME = dshHome;
  mkdirSync(dshHome, { recursive: true });
  // 模型和 Key 全用 DSH 自己的配置：`dsh web` 的 Models 页面里配的服务商（官方 DeepSeek 或任意兼容接口），
  // Key 在 DSH 自己的凭据库里，同席不经手。新版存在当前 profile 的 cordis.patch.yml，旧版存在 settings.yaml。
  const ownSettings = (() => {
    const sections = {};
    let parse;
    try { ({ parse } = requireNative('yaml')); } catch { return sections; }
    const read = path => { try { return parse(readFileSync(path, 'utf8')); } catch { return null; } };
    for (const name of ['settings.yaml', 'settings.yaml.imported']) {
      const legacy = read(join(dshHome, name));
      if (legacy && typeof legacy === 'object' && !Array.isArray(legacy)) Object.assign(sections, legacy);
    }
    const profiles = [process.env.AGENT_HUB_DSH_PROFILE, 'web', 'headless', 'default'].filter(Boolean);
    const profile = profiles.map(name => join(dshHome, 'profiles', name, 'cordis.patch.yml')).find(path => existsSync(path));
    const rows = profile ? read(profile) : null;
    if (Array.isArray(rows)) for (const row of rows) {
      if (row && typeof row.id === 'string' && row.insert === undefined && row.config && typeof row.config === 'object')
        sections[row.id] = { ...(sections[row.id] || {}), ...row.config };
    }
    return sections;
  })();
  const customProviders = ownSettings['llm-pi-ai']?.providers || {};
  // Keep DSH's native coding tools instead of composing an ACP-only chat shell.
  // These are the base CLI tools with the Windows PowerShell executor.
  process.env.DSH_PERMISSION_MODE = coordinatorMode ? 'read-only' : 'danger-full-access';
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
    ['token-meter', 'dsh-token-meter'], ['compaction-pruner', 'dsh-compaction-tool-result-pruner'],
    ['compaction', 'dsh-compaction-basic'], ['command-compact', 'dsh-command-compact'],
    // 0.2.1 起 workflow 引擎从 worker-thread 换成 PTC 运行时，并依赖工作目录服务；按已装版本选接法。
    ...(hasPackage('dsh-workflow-worker-thread')
      ? [['workflow-worker-thread', 'dsh-workflow-worker-thread']]
      : [['working-directory', 'dsh-working-directory'], ['ptc-runtime', 'dsh-ptc-runtime-node'],
         ['workflow-ptc', 'dsh-workflow-ptc']]),
    ['tool-workflow', 'dsh-tool-workflow'],
    ['tool-ralph', 'dsh-tool-ralph'], ['tool-str-replace-editor', 'dsh-tool-str-replace-editor'],
    ['skill', 'dsh-skill'], ['skill-filesystem', 'dsh-skill-filesystem'], ['tool-skill', 'dsh-tool-skill'],
    ['commands', 'dsh-commands'], ['agent-instructions', 'dsh-agent-instructions'],
    ['web', 'dsh-web'], ['web-search-deepseek', 'dsh-web-search-deepseek'],
    ['web-fetch-http', 'dsh-web-fetch-http'], ['tool-web', 'dsh-tool-web'],
  ];
  // 缺了就整个起不来的核心插件；其余插件在新版里没了就跳过，不让一次 DSH 改版把成员整个弄坏。
  const required = new Set(['llm', 'session', 'agent', 'tools', 'system-prompt', 'session-persistence-jsonl',
    'llm-pi-ai', 'agent-loop', 'sandbox', 'sandbox-policy', 'subprocess', 'approval']);
  const plugin = (id, packageName) => {
    const official = officialPlugins.names.get(id);
    const name = (official && resolvePackage(official)) || resolvePackage(packageName);
    if (name) return { id, name };
    if (required.has(id)) throw new Error(`DSH 缺少核心插件 @deepseek-ai/${official || packageName}`);
    skipped.push(id);
    return null;
  };
  const entries = plugins.map(([id, packageName]) => plugin(id, packageName)).filter(Boolean);
  // 已选插件声明依赖、而官方默认组合里有（且不需要配置）的插件，自动补上——新版多出来的依赖服务不用再手写。
  const included = new Set(entries.map(item => item.id));
  const officialIds = new Map([...officialPlugins.names].map(([id, name]) => [name, id]));
  for (const entry of [...entries]) {
    let peers = {};
    try {
      let dir = dirname(entry.name);
      while (!peers.name && dir !== dirname(dir)) {
        try { peers = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8')); } catch {}
        dir = dirname(dir);
      }
    } catch {}
    for (const peer of Object.keys(peers.peerDependencies || {})) {
      const id = officialIds.get(peer.replace('@deepseek-ai/', ''));
      if (!id || included.has(id) || !officialPlugins.bare.has(id)) continue;
      const added = plugin(id, peer.replace('@deepseek-ai/', ''));
      if (added) { entries.splice(entries.indexOf(entry), 0, added); included.add(id); }
    }
  }
  const added = entries.filter(item => !plugins.some(([id]) => id === item.id)).map(item => item.id);
  if (skipped.length || added.length) noteFailure(`插件组合调整：跳过 [${skipped}]，按官方依赖补上 [${added}]`);
  const configure = (id, config) => { const entry = entries.find(item => item.id === id); if (entry) entry.config = config; };
  configure('session-persistence-jsonl', {
    root: join(dshHome, 'sessions'), compression: sharedHome ? 'zstd' : 'none' });
  configure('system-prompt', {
    persona: projectMode
      ? '你是同席项目成员 DSH。方案、实现、验证、文档或收尾职责均以本轮用户原话和当前 assigned_excerpt 为准，不自行承担其他成员的工作。使用原生 CLI 工具处理当前任务，可用 hub_message 联系项目成员、hub_inbox 接收协作消息。'
      : '你是同席中独立的 DSH 私聊成员，负责简单编码建议和小问题分析。像在 DSH CLI 中一样，按需使用可用工具解决用户任务；直接回答用户，不引用其他会话内容。'
  });
  configure('sandbox-policy', {
    mode: 'danger-full-access', workspaceRoot: process.cwd()
  });
  configure('approval', { policy: 'never' });
  configure('permission', { presets: {
    'read-only': { sandbox: 'read-only', approval: 'ask' },
    'workspace-write': { sandbox: 'workspace-write', approval: 'ask' },
    'danger-full-access': { sandbox: 'danger-full-access', approval: 'never' }
  }});
  configure('tool-fs-search', { sampleOverCapGlobResults: false });
  configure('tool-subagent', {
    provider: 'spawn', toolName: 'subagent', backgroundMode: 'continuable'
  });
  configure('tool-subagent-fork', {
    provider: 'fork', toolName: 'subagent_fork', backgroundMode: 'one-shot'
  });
  configure('tool-todo', { allowParallelInProgress: true });
  configure('workflow-ptc', { provider: 'spawn' });
  configure('tool-ralph', { subagentProvider: 'spawn', maxRounds: 64 });
  configure('tool-str-replace-editor', { maxOutputChars: 16000 });
  configure('agent-instructions', { maxBytes: 65536 });
  configure('web', { searchProvider: 'deepseek-official', fetchProvider: 'http' });
  configure('web-search-deepseek', { apiKeyEnv: 'DEEPSEEK_API_KEY' });
  configure('tool-web', { fetch: true, searchTimeoutMs: 60000 });
  // DSH 自己的凭据库（`dsh web` 里填的 Key，或 DEEPSEEK_API_KEY）+ 官方 DeepSeek 服务商；自定义服务商走 llm-pi-ai。
  entries.splice(entries.findIndex(item => item.id === 'llm-pi-ai') + 1, 0,
    ...[['credentials', 'dsh-credentials-local'], ['llm-deepseek', 'dsh-llm-deepseek'],
      ['deepseek-llm-api-extensions', 'dsh-deepseek-llm-api-extensions']]
      .map(([id, packageName]) => plugin(id, packageName)).filter(Boolean));
  // 默认模型：DSH 自己设的默认 → 第一个自定义服务商的第一个模型 → 官方 deepseek-v4-flash。会话里随时能换。
  const [customId, custom] = Object.entries(customProviders).find(([, provider]) => provider?.models?.length) || [];
  const startModel = ownSettings.acp?.provider && ownSettings.acp?.model
    ? { provider: ownSettings.acp.provider, model: ownSettings.acp.model }
    : custom ? { provider: customId, model: custom.models[0].id }
    : { provider: 'deepseek-official', model: 'deepseek-v4-flash' };
  entries.push({ id: 'acp', name: requireNative.resolve('@deepseek-ai/dsh-acp'), inject: ['agentLoop'], config: startModel });
  if (coordinatorMode) {
    // Permission presets depend on a shell executor. The tool-free coordinator
    // deliberately has none; keeping that plugin pending makes boot's activation
    // audit dispose ACP after initialize, before session/new can complete.
    const allowed = new Set(['llm','session','agent','jobs-local','tools','system-prompt','session-persistence-jsonl','session-projection','llm-pi-ai','credentials','llm-deepseek','deepseek-llm-api-extensions','agent-loop','sandbox','sandbox-policy','subprocess','approval','token-meter','compaction-pruner','compaction','acp']);
    for (let index=entries.length-1;index>=0;index--) if (!allowed.has(entries[index].id)) entries.splice(index,1);
    configure('system-prompt', {persona:'你是同席项目的隐藏调度管家。只返回引用用户原文和成员结果的路由JSON，不编写方案、代码或额外成员指令；没有任何项目、shell、文件、MCP、技能或委派工具。'});
    configure('sandbox-policy', {mode:'read-only',workspaceRoot:process.cwd()});
  }
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
  // DSH 自己的服务商与设置在内存里叠上去，不写进磁盘上的组合文件。
  const overrides = ['llm-pi-ai', 'llm-deepseek'].filter(id => ownSettings[id] && entries.some(item => item.id === id))
    .map(id => ({ id, config: ownSettings[id] }));
  ctx = await boot('agent-hub-dsh', config, overrides, host => {
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
