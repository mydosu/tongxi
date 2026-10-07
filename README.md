# 同席 · Agent Hub

个人独立 Windows 桌面多 Agent 协作软件，当前版本 `0.12.14`。已完成本地会话管理、四位成员的真实私聊与群聊讨论、当前会话内模型与思考强度切换、无边框窗口与 HUD 浮窗，以及角色可配的项目协作。

代码在 `src/`（TS）与 `src-tauri/`（Rust），构建见「开发与测试」。采用 Tauri 2 + TypeScript + Rust + SQLite，使用系统 WebView2。默认数据库位于 Windows 应用本地数据目录 `local.personal.agenthub/hub.db`，完整路径可在“成员与服务”页查看。旧版数据会事务迁移至当前 schema（v10），保留消息、设置和原生会话映射；迁移前通过 SQLite 保存一份完整旧数据库到同目录 `backups`，包含已提交的 WAL 数据。本机构建产物与冻结的历史版本放在 `release/`，不进版本库。

## 使用

1. 选择 Codex、Hermes、DSH 或阿尔比恩私聊，点击连接。
2. 在消息上方直接切换模型与思考强度，选择后自动保存；群聊可先选择成员。下一条消息使用新配置，正在生成的回复保留开始时的参数。右上角“模型设置”也可使用。
3. 输入内容，点击“发送给成员”或按 **Enter**，**Shift+Enter** 换行。中文输入法组合与确认事件不会触发发送。
4. 回复流式显示并保存。“停止回复”等待原生中断确认后恢复发送。
5. 软件重启后重新连接，继续发送即可恢复该私聊的原生上下文。

群聊中勾选二至四位参与成员，选择 1—3 轮，点击“连接参与成员”，输入需求后点“开始讨论”或按 **Enter**。成员按会话列表顺序发言，后发成员会收到前面的公开回复，下一轮继续交流；每次发言使用该群该成员的模型设置。阿尔比恩在新群中默认不勾选，需要显式邀请。保存草稿仍只写到本机，不发送给模型。

“停止讨论”取消当前原生回复并阻止后续成员和轮次。等待停止确认；超时则断开对应的同席子进程。失败后保存已经公开的回复，后续发言不继续启动。重启保留讨论和独立原生会话，未完成讨论标为中断，不自动重放；可以发送新需求继续。

窗口没有系统标题栏。拖动左上角品牌区域或会话头部空白处移动窗口，双击这些区域最大化/还原；右上角保留最小化、最大化/还原、关闭按钮，窗口边缘保留原生缩放。

## 项目协作

在任意群聊里点击“绑定项目”，选择要协作的实际目录。连接成员后，在当前输入框描述需求，点击“执行项目”。讨论和项目执行是两个按钮；Enter 用于发送讨论，Shift+Enter 换行。

**第一步先配角色。** 弹出面板让你为**规划／执行／验收**各选一位成员、模型与思考强度（留空＝由该成员自动选型）。成员只列出当前会话里已连接、且真能承担该角色的：执行角色只能是 Codex 或 DSH（只有它们有文件工具），规划与验收还可以是 Hermes。选好点“开始规划”——这一步**只出方案**，状态停在“等待你确认方案”，写入租约继续握在手里。

**第二步逐项确认。** 方案产出的每项任务都给出实现者、授权文件与依赖；面板上每个任务都能改模型与强度（预填方案的选型建议）。点“确认并开始执行”才开始动文件。

**执行。** 每个任务在自己的 **git 工作树**里由独立子进程完成，互不依赖的任务并行；有依赖的等前置任务完成，并且它的工作树以那个前置任务的分支为基线，所以能看到前置的成果。任务完成后只把该任务授权的文件提交到任务分支。

**合并与验收。** 全部任务通过后，按任务顺序把各分支合并回原分支——任何一个冲突都**立刻停下、保留现场**，不自动解法、不回退、不强推。合并后由**验收角色**读实际源码给出结论：通过才算完成，不通过就交回执行角色修复一次，再验收，仍不过就停下并保留证据。结论来自实际代码，不接受模型的“已完成”声明。

项目必须是 Git 仓库才能绑定和执行；普通目录会在绑定时明确拒绝。请先在项目根目录初始化 Git 仓库并提交基线。

任务卡显示每项任务的状态、实际执行参数、工作树与分支，以及实际文件记录。“停止协作”停止当前任务并阻止后续步骤；软件重启后恢复已有记录，未结束的协作标为中断，不自动重放。

当前最多两项项目协作。同目录或父子目录项目共享写入排斥，冲突任务等待；同一成员不能同时执行项目步骤和聊天回复。项目尚未结束时，技术成员不能被断开。文件工具仅开放方案授权的文件，拒绝凭据路径、越界及链接；验收脚本不能由实现者修改。修改前保存原文件和元数据，这些记录不等于自动回滚或操作系统级沙箱。实际检查程序按用户配置的权限运行。阶段验收仅使用框架自建的隔离项目，没有自行修改用户已有项目。

## 模型与思考强度

设置按 **会话 × 成员** 存入 SQLite。群聊中的成员分别保存，已有私聊设置保持独立；修改和恢复默认不会删除聊天历史，也不修改 agent 的全局配置。回复期间可以修改下一轮设置，当前轮参数在开始时冻结；已归档会话不能修改。保存期间暂停发送，避免新消息使用尚未保存的参数。

项目协作里每个角色的模型与强度有三个来源，优先级是**任务级覆盖 > 角色面板里选的那一项 > 该成员自动选型**；规划给出的选型建议只在自动选型时作为兜底。正在执行的步骤保留开始时的参数。文件工具只给执行角色：规划与验收只读，执行与修复才能写。Codex 在项目模式下一律不带插件与外部 MCP，只给框架的受控文件工具。

连接 Codex 后，模型和强度选项来自 `model/list`。模型留空时沿用连接时读取的本机有效默认模型/强度；显式选择模型、强度留空时采用该模型目录给出的默认强度。发送时再次校验能力，将模型与 effort 传给 `turn/start`。切换模型继续使用原 thread，恢复默认会明确覆盖之前的 turn 设置。

连接 Hermes 后，模型来自本机原生模型目录，包括提供商路由。薄适配层把思考强度赋给当前 agent 的 `reasoning_config`；“默认”恢复该模型按 Hermes 配置得到的初始设置。Hermes 会按模型和提供商映射思考等级，可能下调或忽略不支持的等级，因此不保证每个等级原样出现在提供商请求中。

DSH 复用本机 Hermes 里配置的两条路由（代码按 provider id `command-code-daily` 与 `command-code-daily-2` 匹配）。切换选项代表切换 API 线路；默认使用第一条线路及 low 强度。支持 off / low / high / max 选项，通过原生 ACP `session/set_config_option` 应用到同一个 native session。目录中的能力声明不代表各等级已逐一验证提供商实际执行，真实验收覆盖 off 和 low。

阿尔比恩使用 WSL 内原有 albion profile 的模型和认证。连接后读取原生目录，模型／强度设置只影响本会话下一次发言。软件加载她原有的 SOUL 人格文件，不修改源人格文件。受邀群聊使用独立原生 session，不导入任何私聊。

目录可见不等于账户有调用权限，以真实请求成功为准。断开连接后可以保存待验证的设置，发送时再次校验；失效的设置会产生明确失败状态。

## 已实现的边界

- 四个成员：Windows Hermes、Codex、Windows DSH、WSL 阿尔比恩。三类用法：私聊、群聊讨论、项目协作。项目协作的**规划／执行／验收三个角色可分别指定成员、模型与思考强度**（留空即自动选型），执行前先出方案、由你逐项确认任务与参数后才开跑；每个任务在自己的 git 工作树里由独立子进程执行，互不依赖的任务并行，全部通过后按顺序合并回主分支，冲突立即停下并保留现场。阿尔比恩为陪伴与开发知情。
- 私聊、二至四位成员群聊、改名、搜索、归档、恢复、确认删除、成员管理。草稿和上次选择在重启后恢复。
- 真实群聊讨论任务：一条用户需求对应多成员／多轮真实回复，数据库分别保存每次运行与整场讨论，显示已完成发言数及当前成员。当前最多一场群讨论，同时允许空闲成员在其他私聊回复；同一成员不能同时生成两条回复。
- 群提示仅包含该群已提交用户消息和公开回复，排除未发送草稿、其他群和全部私聊。调度器构造的公开记录上限为 64,000 字符；历史单条最多 12,000 字符，超长会明确标记截断，当前需求完整保留。原生 harness 自身的会话历史另由其管理。
- Codex 私聊与项目角色，按被分配的角色出方案、改文件、修复或验收。原生 `thread/start`/`thread/resume`、流式事件、`turn/interrupt`；运行信息与会话参数保存到本机。项目模式下不给插件与外部 MCP，只给受控的文件工具。
- Hermes 真实 ACP 私聊，当前为**工具关闭的聊天模式**，项目模式按被分配的角色出方案或验收（这条路径没有文件工具）。独立原生 SessionDB 位于同席数据目录 `hermes-native/sessions.db`。
- DSH 真实 ACP 私聊，复用安装的 harness，当前为工具关闭的聊天模式。独立原生会话位于同席数据目录 `dsh-native/sessions`，支持原生续聊、中断和重启恢复。
- 阿尔比恩真实私聊走 WSL 内由她自己的服务进程提供的本机 OpenAI 兼容端点（默认 `127.0.0.1:8650`）；人格、记忆与工具都在该服务里，同席只当客户端。软件不启动也不停止该服务，新建私聊分别保存自己的原生会话，不导入原有私人聊天。
- 阿尔比恩回复中原语音系统的结构化情绪控制标记不会进入聊天正文；流式分片期间也会隐藏未完成标记。用户输入保持原文。
- 四位成员各自最多一个同时运行的 turn，可分别连接或断开；消息和状态按成员隔离。运行中的会话禁止归档、删除或更改成员。
- 更新管理：DSH 与 Codex 可检查 npm 发行版、准备隔离候选、原生预检后启用并回退；Hermes 由其自身更新通道处理，同席只读检查；阿尔比恩因 WSL 安装含本地定制，仅检查本地版本，不自动覆盖。
- 退出软件清理软件创建的原生进程树。重启时把未完成运行标为中断，不自动重放请求。
- 原生接口通过 stdio JSON RPC 通信，Tauri 事件传递增量，SQLite 保存结果。递增 revision 避免旧状态覆盖新状态。

Hermes 与 DSH 薄适配脚本嵌入 EXE，复用本机安装的 Python / Node / 原生 harness，不修改安装源码。Codex 和 Hermes 使用已有认证；DSH 的两条路由认证由 Hermes 原生解析器只读解析，通过私有子进程管道及临时进程环境传递，不输出到界面、日志或验收报告，不保存到同席配置或凭据文件。认证在连接时解析，修改外部凭据后需要重新连接。软件不改变全局模型设置，也不导入旧桌面会话；没有改动现有 agent 的技能、服务和自启。

阿尔比恩的原生数据库放在 WSL 的 `/root/.local/share/local-agent-hub/instances/<namespace>/albion-native/sessions.db`。Windows 数据目录的 `albion-wsl.json` 保存无凭据的 namespace、发行版和 profile 绑定；软件不依赖 Windows 盘符自动挂载。完整备份需要同时保存 Windows 数据目录和对应 WSL instance。更换发行版或 profile 时使用独立数据目录，避免混用原生会话。源 profile／安装源码在该私有子进程中设置写入保护，原生人格加载时明确绑定 profile，避免独立 SessionDB 导致人格目录误判。

Codex 参数依据 [OpenAI App Server 官方文档](https://developers.openai.com/codex/app-server)，并以本机生成的协议 schema 核对；验收还只读核对了本次新建测试 thread 的原生 turn-context 模型/effort 字段。Hermes 接口与映射依据本机安装源码。DSH 接入依据 [DSH 官方 ACP 文档](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/acp/acp/README.md)及本机安装源码。

## 开发与测试

```powershell
npm ci --no-audit --no-fund
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path src-tauri/Cargo.toml --check
npm run desktop
npm run desktop:build
```

构建需要 Rust/MSVC、Node 和 WebView2，见 [Tauri 前置条件](https://v2.tauri.app/start/prerequisites/)。构建后自动复制程序至 `release/Agent Hub.exe`，生成大小和 SHA256 清单 `release/manifest.json`。依赖锁定于 package-lock.json、Cargo.lock。

Codex 从 PATH 中查找原生 exe，也支持 `AGENT_HUB_CODEX_EXE` 指定绝对路径。Hermes 默认发现 `%LOCALAPPDATA%/hermes/hermes-agent/.venv/Scripts/python.exe`，也支持 `AGENT_HUB_HERMES_PYTHON`；该 Python 必须属于带有原生 ACP 模块的 Hermes 安装。DSH 与 Codex 的安装目录默认从 PATH 上的 `dsh` / `codex` 反推（CLI 壳同级的 `node_modules/<scope>/<包名>`；npm -g 全局前缀与自带 `node_modules` 的独立安装都适用），也可用 `AGENT_HUB_DSH_INSTALLATION` / `AGENT_HUB_CODEX_INSTALLATION` 直接指定。Node 从 PATH 查找，可用 `AGENT_HUB_DSH_NODE` 指定。两条 Hermes 路由必须存在，可用 `AGENT_HUB_DSH_HERMES_REPO` 指定 Hermes 仓库。软件自身不安装或更新这些 harness。

阿尔比恩默认连接本机 `127.0.0.1:8650` 的 OpenAI 兼容端点，可用 `AGENT_HUB_ALBION_ENDPOINT` 指定；该服务读取的 `.env` 可用 `AGENT_HUB_ALBION_ENV` 指定。服务由她自己的进程启停，同席只做客户端。

测试脚本需要本机 Python Playwright；系统输入测试还需要 Pillow 与 Win32 API：

- `python scripts/probe_codex.py`、`python scripts/probe_hermes.py`：协议与模型能力探测，不发送模型请求。
- `python scripts/smoke_desktop.py`：发布 EXE 会话管理回归，使用 WebView2 CDP，不调用模型。
- `python scripts/verify_stage6.py`：真实 v5 数据迁移与备份、两成员多轮交换、原生公开输入证据、私聊隔离、模型与 DS 双线路切换、四成员参与、受邀阿尔比恩、重启续聊、停止、失败及崩溃恢复。
- `scripts/Invoke-NativeVerification.ps1 -VerificationScript verify_stage6_native.py`：真实系统鼠标键盘选择成员、连接、设置、两轮讨论、停止、中文草稿与会话切换；CDP 仅观察。
- `python scripts/verify_stage4.py`：发布 EXE 的会话内切换、当前轮参数冻结、三个真实 harness，以及 DSH 双线路/隔离/重启/取消/恢复验收。
- `python scripts/verify_stage5.py`：真实旧版数据库迁移／备份、阿尔比恩人格、Windows Hermes 独立、私聊隔离、重启续聊、取消、崩溃清理、源 profile 与既有网关完整性。
- `scripts/Invoke-NativeVerification.ps1 -VerificationScript verify_stage5_native.py`：真实鼠标键盘连接阿尔比恩、选择会话模型／强度、发送一条真实回复、恢复默认、断开与退出。
- `python scripts/probe_albion.py`：WSL 原生 ACP 探测，不发模型请求；`--prompt` 额外发一条人格探测，报告只保存布尔结果和非敏感元数据。
- `scripts/Invoke-NativeVerification.ps1 -VerificationScript verify_stage4_native.py`：真实鼠标键盘验收会话内模型/强度、成员隔离、中文草稿与恢复默认。复用下述已授权的共享暂停恢复流程，不调用模型。
- `python scripts/verify_stage3.py`：发布 EXE 的设置、真实 Codex 模型切换/恢复默认、Hermes 私聊/隔离/重启/取消验收；使用 CDP 自动化。
- `python scripts/verify_stage3_native.py`：真实系统鼠标键盘验收窗口拖动、缩放、窗口按钮与会话设置；不调用模型，CDP 仅定位和观察。
- `scripts/Invoke-NativeVerification.ps1 -VerificationScript verify_stage3_native.py`：本机 Mouse Without Borders 干扰焦点时，**经用户明确授权暂停共享之后**使用。仅暂停助手提升权限，软件保持普通权限；结束、宿主退出或超时后恢复，不修改设置。
- 第二阶段脚本 `verify_codex_chat.py` / `verify_native_input.py` 保留，分别使用 CDP / 系统输入验证 Codex 私聊。

测试使用 `artifacts/desktop-test-*` 隔离本地数据，仅测试子进程打开临时本机 CDP 调试端口。普通启动不打开调试端口。软件不依赖键鼠测试工具或共享暂停助手。

## 后续

当前版本 `0.12.14`。已接通：项目目录绑定、角色可配的两段式项目协作、每任务独立 git 工作树与真并行（依赖排序、全部通过后自动合并、冲突停下保留现场）、四家共用真实会话库、一键打开各端客户端、HUD 浮窗、服务安装与更新管理（含维护锁）。验收脚本都在 `scripts/`。

核心功能与当前验收已完成：v0.12.14 的实体 HUD 拖动 3/3、实体托盘图标点击恢复 3/3、Codex 规划／执行／验收 GUI 项目流程 10/10；并行、审核打回后一次修复、合并冲突保留现场也已完成真机验收。HUD/托盘窗口状态自动验收为 47/47 与 8/8。后续可独立追加会话树视图、跨会话互相提问、知识库／代码图谱、额度与负载可见、手机端。

## 许可证

MIT
