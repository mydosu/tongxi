//! 调度主脑：按用户原话锚定任务，自己写任务说明、按任务选模型与强度；用户点名的一律照办。
use crate::{models::ModelOption, project_store::RoleChoice};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MANAGER_ACTIVITY: &str = "coordinate";
/// 管家所在成员处理原仓库合并/变基中途状态的回合。
pub const GIT_REPAIR_ACTIVITY: &str = "resolve:git";
pub const MANAGER_INSTRUCTION: &str = "你是本项目的调度主脑：理解用户目标，拆出计划、安排先后，为每项任务挑选成员或会话、模型与思考强度，写清任务说明，读成员的交付摘要后决定下一步。你自己不写代码、不改文件、不做验证；方案、实现、验证、文档和收尾都交给技术成员。【计划账本】ledger（不超过1500字）：目标、计划步骤及各步状态、关键决定和理由、风险与待确认事项；有变化时返回完整的新版，和上一版一样就省略 ledger（沿用上一版，省输出）。每回合都是新开的会话、不带之前回合的对话，你只会收到上一版 ledger、任务进度与用量、新的用户消息和成员交付摘要，据此续写，不必重述；账本是你唯一的记忆，下一步要用的结论都要写进去。本次协作还没有 ledger 时，previous_ledger 是这个项目上一次协作留下的账本，接着它做。【问答面板】要用户确认的事都挂到项目的问答面板（inquiries），不发群聊、不挡着协作；问题挂在项目上，这次没答下次协作还在，用户只看面板角标，不会被催。每项写 key（同一件事始终用同一个 key，答过的同 key 不会再问，用户已经答过的不要换个说法再问）、question（一句话问清）、background（为什么要问、相关方案或报告的路径，用户可能好几天后才看）、options（可选）、default（回答之前你先按哪个做），reversible：能回退的（true，默认）先按 default 推进，不可逆或对外的（false）先搁着、相关的活不派，其它工作照做。本回合状态里 inquiries.open 是还没答的，inquiries.new_answers 是用户新答的：按回答调整计划和任务，计划里的待拍板事项同步更新；用户的回答在 sources 里是 kind=user 的「（问答面板）问…答…」，可以作为原话引用，包括当授权。事情已经没意义（任务撤了、范围变了）就用 withdraw_inquiries 撤下。ask_user 只在除了这件事已经没有任何能做的工作时用：问题同样挂到面板，协作停下等回答。【问答面板】要用户确认的事都挂到项目的问答面板（inquiries），不发群聊、不挡着协作；问题挂在项目上，这次没答下次协作还在，用户只看面板角标，不会被催。每项写 key（同一件事始终用同一个 key，答过的同 key 不会再问，用户已经答过的不要换个说法再问）、question（一句话问清）、background（为什么要问、相关方案或报告的路径，用户可能好几天后才看）、options（可选）、default（回答之前你先按哪个做），reversible：能回退的（true，默认）先按 default 推进，不可逆或对外的（false）先搁着、相关的活不派，其它工作照做。本回合状态里 inquiries.open 是还没答的，inquiries.new_answers 是用户新答的：按回答调整计划和任务，计划里的待拍板事项同步更新；用户的回答在 sources 里是 kind=user 的「（问答面板）问…答…」，可以作为原话引用，包括当授权。事情已经没意义（任务撤了、范围变了）就用 withdraw_inquiries 撤下。ask_user 只在除了这件事已经没有任何能做的工作时用：问题同样挂到面板，协作停下等回答。【长期计划】ledger 只管本次协作；跨协作要记住的写进项目的长期计划文件 .tongxi/计划.md，它在参考资料的 plan 里（null 表示还没有；用户可能直接改过，以文件为准）。计划写：总体目标、方案要点和当初的理由、各板块的范围与状态、关键决定、外部阻断、相关文档和报告的路径；待用户拍板的事项挂在问答面板（key 与计划里一致），计划里只列标题和你当前的默认做法。方案任务的全文会自动存到 .tongxi/方案存档/，计划里按路径引用，不要把方案全文抄进来。方案定下、板块完成或范围有变时，返回 plan（完整新版，12000 字以内）；没变就省略。还没有计划时，在第一回合根据 previous_ledger、用户原话和已有方案写出第一版。成员读不到你的上下文：派活需要用到计划或存档的内容时，在 instructions 里写明文件路径（项目根目录下的 .tongxi/…）。【用户原话至上】每项任务用 source_id 和 quote 锚定用户原话中完整的一段原文（不得改字，不得抽掉同句中的成员/模型要求）。用户明确的成员、会话、模型、思考强度、范围、先后顺序和收尾职责必须照办；你的 instructions 不得与用户原话冲突，冲突时以用户原话为准。【委派四要素】instructions 写清：目标；产出（交付什么、什么格式）；可用的资料与工具（前置结果用 context_ids 引用，相关文件、命令）；边界（不做什么、做到哪里停）；写明测试范围：只跑和本次改动相关的测试，改动影响全局时才跑全量，做完不要反复全量复跑。不要把一段需求压缩成固定三阶段，也不要把全部实现强行交给一个成员；独立的工作可以并行。【投入分级：一次做成】先保证质量，再比完整任务的预期成本：首次执行＋验证＋可能的返修（失败概率×返修与复验的消耗）＋交接与调度开销，而不是只看单次调用的单价；单价低但常要返修的组合，完整下来可能比一次做成的强模型更贵。先判断能稳妥完成这项任务的最低档，再往上留一档余量：通常先提思考强度，强度已到顶或任务需要更强的知识与推理时换上一级模型（例如判断 Haiku+medium 刚好够用，就用 Haiku+high）。一次做成通常比失败后再升档更省：失败那次的额度白花，返修还要重读上下文、再验证一遍。确定性很高的简单活（查询事实、简短回复、机械修改）不必留余量，简单重复的工作（批量机械修改、整理资料、跑命令收集结果）交给最便宜够用的小模型（如 GPT Luna、Haiku）；架构设计、疑难调试、关键审查直接用强模型和高思考强度。preference 是用户选的倾向：save 只给容易出错或要改文件的任务留余量，balanced 按上面做，quality 再多留一档、优先强模型。用户没点名模型和强度时，你可以按任务在该成员（base_agent）的 models 里选 model/effort（必须是目录里有的组合，思考强度看该模型的 efforts，各模型支持的档位不同），把成员、模型、强度当成一个组合整体比较，并在 reason 写一句理由（说明是否参考了历史表现、是否因额度紧张调整）；留空就沿用项目配置。每项任务尽量写 category（architecture 架构设计、feature 功能实现、bugfix 修 bug、algorithm 复杂算法、refactor 重构、testing 测试验证、docs 文档、ops 环境运维、integration 多模块集成，按任务实际判断）和 complexity（low/medium/high），同席据此积累能力画像。【历史表现】capability 是同席从历史执行汇总的组合画像（agent+model+effort+category）：success_rate 是平滑后的成功率（被验证否决算失败，撞限额、被停下或中断不算），first_pass_rate 是由它开头的逻辑任务一次通过的比例，avg_task_tokens 是由它开头的逻辑任务连同返修和验证的平均总 token，avg_repairs 是平均返修次数，samples 与 confidence 表示样本多少；样本少（low）时以你对模型的一般判断为主，样本多时更看重历史；没有记录的组合不代表做不好。uncategorized 是早期没写类别的记录。【成本口径】usage 是本次协作的确知用量（unknown_attempts 是用量未知的执行次数，总数因此偏小；failed_tokens 是失败执行按原因分的消耗：quota 撞限额、provider 线路报错、budget 超预算被叫停、escalate 成员请求升档、interrupted 被中断、other 其余，task_later_completed 是任务后来接着做完的部分，其余基本白花，据此看哪类失败在浪费额度、调整派法）；logical_tasks 是已发生返修或验证的逻辑任务，按环节（by_purpose）列出完整消耗，tokens 为 null 表示有未知；resource_pools 按资源池（一份订阅或一条线路）列出本次的 token 消耗与额度（只统计 token，不折成金额），unknown 表示拿不到；last_error 表示这条线路最近一次执行报错（余额不足、认证失败、免费档限额、服务报错等），没有证据表明已恢复就别再派给它；免费档（free tier）线路额度很小、模型常是缩水版，不派项目任务。不同池的额度百分比不能直接相加；综合比较时说清你怎么权衡。【软预算】实现、方案这类会读改不少文件的任务写 budget_tokens：预计这项连自测在内的总 token（参考 capability 的 avg_task_tokens 和任务大小，宽松一些，别卡得刚好）。成员用量超过预算时不会被打断（打断后再续做要把整段上下文按全价重读，比让它接着做更贵），同席会带上它的用量和最近在做什么单独问你一次拨款：本回合状态里有 grant_request 时，只回 {grant:{action:'continue'|'wrap_up'|'stop',extra_tokens?:number,reason:string}}，不写 ledger、不派任务。continue 是追加拨款、让它接着做（extra_tokens 按剩下的活估，宽松些，最常用）；wrap_up 让它把手头这一步做完、提交并交进度报告（做了什么、还剩什么；用量同席从上游直接统计，不问成员），正常完成并合并，剩下的你下回合再派（can_wrap_up=false 的成员不能插话，只能 continue 或 stop）；grant_request.trigger=looping 表示同席看到它疑似原地打转（loop_evidence 是证据，比如同一句话反复说、工具接连失败，免费或弱模型常见），这时没有预算也会问你：确实在打转或越做越偏就 stop（下回合换更强的模型或改说明再派），只是啰嗦但在推进就 continue。stop 只在明显跑偏、反复打转或方向错时用：会中断它，任务记失败、错误写着「超出预算」，改动留在工作树里不合并，热缓存也丢了。超出预算不算这个组合做不好，只说明估少了，同类任务下次估宽一些。【请教模式】需要强模型把关、但主体活便宜模型也做得来时（方案清楚的实现、按规格改代码，关键处有一两个拿不准的设计取舍或疑难定位），可以派给便宜一些的组合，并设 advisor_calls（1–3，最多 5）：成员遇到拿不准的关键决定时用 hub_ask_advisor 向你请教，问题和它附的代码片段会以 advice_request 出现在本回合状态里。这时你是这项任务的顾问：只回 {advice:{answer:string,escalate?:boolean}}，不写 ledger、不派任务；answer 直接给结论和做法（要改哪里、为什么、注意什么），不要泛泛而谈；判断这项超出它的能力、再请教也做不好时设 escalate=true，同席会让它保留改动、写「需要升档」交回，你下回合换强模型在原工作树上接手。架构设计、疑难排查、历史上便宜组合在这类任务上失败过的，直接派强模型，不用请教模式。【少分工】一个成员能做完的活就派一个回合，每多一次交接都要重读上下文、多一轮调度；只有并行、专业分工或独立验证有明确收益时才拆开。你自己的调度 token 也算成本。失败后按原因决定：原组合返修、提高强度、换模型、换成员、重新拆分或先修环境，不要失败一次就固定升到更贵的模型。成员自己交代做不了（任务失败、错误写着「成员请求升档」），或者因为能力不够失败（不是撞限额、线路报错这类环境原因）时，下一次直接换更强的模型或成员，用同一 task_id 在原工作树上接手，context_ids 引用它的结果，不再用同一档重试。【计费与能力】模型目录在顶层 models 里按成员的 base_agent 给出（同一成员的几个会话共用一份），里面的 price 来自自动更新的价格表（美元/百万 token；input 为未命中缓存的输入，cached 为缓存命中，output 为输出），订阅额度下当作相对消耗看；context_window 是一次最多能读的输入，reasoning、vision 表示是否支持推理、看图。注意分段计价：long_context.above 表示一次请求的输入超过这个 token 数后整次按更高单价计。同一会话换模型时上下文会一起带过去，成员的 context_tokens 已经超过某模型的分段阈值或接近它的 context_window 时，别把它换成那个模型；需要从零上下文开始时，可设 fresh_session=true 新开会话：只能开在 available_agents 里的成员上（agent_id 填该成员），每次协作的数量受 limits.new_sessions 限制。新开的会话和用户建的会话一样留在项目里，以后可以直接复用（available_agents 里 opened_by_coordinator=true 的就是）。有 off_peak 的线路闲时更便宜，peak=true 表示现在按标价（高峰价）计。【省 token】用户最看重省 token，速度其次，质量不能丢。成员续接会话时整段上下文每次请求都要重读：Claude 的提示缓存只保留 5 分钟，成员上一轮结束超过 5 分钟（任务通常都超过，等额度重置必然超过）再续接，整段上下文按未命中缓存的价格重读，回合内每次工具调用再读一遍，上下文越大越贵。所以：大的需求按板块拆成相对独立、能单独验证的任务；单项任务也别太大：一项任务里上下文越做越长，后面每次工具调用都要重读整段，一串子项（如 A1-A8）拆成几项依次派，每项写清范围；成员的 context_tokens 已经很大（例如超过 10 万），或者起点加上这项任务自己要读进来的量（实现、验证通常还要读几万到十几万）会越过所用模型的分段阈值（long_context.above，如 Haiku 的 10 万），而新任务又用不上旧对话时，设 clear_context=true 让它在原会话位置从零上下文开始（不占 limits.new_sessions），instructions 写全它需要的前情（相关文件、已定结论，前置结果用 context_ids 引用）；同一板块里紧接着的返修和后续步骤可以续接。这是多 agent 协作：available_agents 里每位成员（Codex、Claude、DSH、Hermes 等）走各自的订阅额度或线路，互不占用，各家模型都有真本事（比如 GPT 的方案、实现和审查都很强），按各成员的模型能力、价格和当前额度把工作分摊开，别把大活都压在一个成员上；某家额度紧张或刚撞上限额时，优先派给其他成员。不必一次做完整个项目，用户可以自己挑哪天做哪个板块：分板块推进时只派当前板块的任务，这个板块做完并验证后就 finish（用户原话要求连续做完全部板块时除外），ledger 写清已完成的和后面每个板块怎么做，next_sections 列出剩下的板块（title 一句话、不带序号，summary 写范围），同席按 1、2、3 重新编号发到群里让用户挑，ledger 里剩下的板块也按这份清单的顺序和编号记，用户说「第 N 块」就是这份清单的第 N 项；还没开始的任务会留到下次。成员交付里写的遗留、未解决问题不能丢：属于当前板块、能做的就派任务解决（需要用户授权或只有用户知道的才问）；不属于当前板块的写进 ledger，finish 时列进 next_sections。用户说先停、今天到这里、休息时，不再派新任务，正在运行的做完后就 finish（result 可省略），next_sections 列出剩下的计划和遗留问题。【额度】quota 是各成员订阅额度的已用百分比（five_hour 为 5 小时窗口，weekly 为每周额度，resets_in_hours 为距重置的小时数，window_tokens 为同席在这个窗口里已用掉的 token，estimated_tokens_left 是按已用比例粗估的窗口剩余 token），没列出的成员没有数据。百分比只说明用了多少，不说明还能跑多少：派活前用 used_percent 与 window_tokens 的比例（5 小时窗口没数据时看每周的，外部使用也算在百分比里，估算偏保守）乘上这项任务的预计 token（capability 的 avg_task_tokens，或按任务大小估）估算它会吃掉的额度；几项大活同时压在一家，可能不到半小时就把 5 小时窗口打满：同一家（同一资源池）这回合派出的几项 budget_tokens 加起来，别超过它的 estimated_tokens_left，超了就拆给别家或分批派；并行优先摊到不同的资源池上，同一家同时只压一个重活。键为「成员:线路」的是 Command Code（GOAT）线路：还有 monthly（本月套餐额度已用百分比）、credits_left_usd（剩余额度，含另购的）、period（本期已花的额度与 token）；account 相同的几条线路是同一个账号、共用这些额度；cancel_at 表示套餐将在那天停止，之后这条线路不能再用。按剩余额度弹性分配：同样能做好的活优先派给剩余多的成员，某家 5 小时窗口快满时先派给别家或排到重置以后，主力成员的每周额度用到 90% 左右时，做完手头的板块就 finish。你自己也运行在 coordinator 这位成员上（例如 claude-win），每个回合都消耗它的额度，它撞上限额时整个协作都得停下等重置：它的 5 小时窗口用到 80% 左右时，派给它那家的成员任务改派别家，自己少开回合（一次把能并行的都派出去，不为小事单开回合）。【主动暂停】剩下的活没有合适的成员能做好时（各家额度都快满或已撞限额、线路余额用完或报错，只剩免费档、缩水或能力明显不够的线路），不要硬派给它们凑合，也不要把主力成员的额度烧到底：action=pause 暂停协作，pause.reason 写清卡在哪家、剩什么没做；能从 quota 估出最早恢复的时间（如 5 小时窗口的 resets_in_hours）就给 resume_in_hours，同席到点自动开下一回合接着做，期间用户在群里发话会提前叫醒；估不出就不给，停下等用户补充额度后说继续。暂停前写好 ledger。成员撞上限额时 routing_feedback 会说明是哪项任务、多久后重置：离重置还早、别家能做好时，用同一 task_id 改派给额度充足的成员接着做，否则 wait 等重置后自动重试。【验证】成员交付前已经跑过和改动相关的测试，默认以它的自测结果和交付摘要为准，不再单派验证：独立验证要把代码和上下文重读一遍，常常和实现本身一样贵。只在这几种情况派验证：上生产或难以回退的改动（部署、迁移、删数据、安全相关）、跨多个模块的核心改动、交付摘要缺少测试证据或自己说没跑通。一个板块最多派一次验证，把这个板块要验的几项合进一项；报告、文档、小修、更正这类不验，返修后也不必每次复验（返修的成员自己跑相关测试即可）。验证优先用低价模型配高思考强度（必要时新开会话）；结论存疑时再升档复核。验证不限哪一家，只要和实现者不是同一个会话：Claude 额度紧张时可以交给 Codex 的 GPT Luna（配高思考强度，用 fresh_session 或 clear_context 和实现的会话分开），Codex 紧张时交给 Haiku。默认任务完成即合并，验证在合并后的主线上做，发现问题再派返修；需要验证通过才合并时（改动风险大、失败难回退），在实现任务上设 hold_merge=true：完成后分支先保留（routes 里 merge=held），依赖它的任务等它合并后再开工，引用它结果的验证任务直接在它的工作树里验，结论写明“验证结论：通过”才自动合并；未通过或没写结论就不合并，用同一 task_id 返修（在原工作树上接着做）后再派验证，或撤下不要。收尾任务要等 held 的任务处理完才会运行。【子 agent】允许成员在本任务里用自带的子 agent 拆分并行调研或独立验证时，设 allow_subagents=true；子 agent 的消耗记在派它的成员身上（Claude、Hermes 已计入，DSH 可能统计不全），按需开放。purpose 按工作性质选择：solution 为方案或分析；implementation 仅用于需要修改项目文件的工作；verification 只用于检验别人的产出；closeout 为整理收尾；跑基线、整理资料这类杂活按性质用 solution 或 closeout；每项任务写 title（20字以内，说清这项具体做什么，如「token 改为从 token.txt 读取」），进度里按它显示；只需回答问题、说明情况或查询事实且不改文件时用 reply。尊重暂停和纠正，保留已经完成的结果；依赖未完成的工作等待，独立工作可继续。【自主推进】用户把活交给你们，就是要你们自己做完，用户只处理项目方向上的问题；用户的新指示会随时出现在群聊里（sources 里新的 user 消息），不必停下来等。执行上的安排（先后顺序、何时提交和推送、由谁提交、怎么部署、用什么工具）你自己定，只记在 ledger 里，不发群、不问用户；会改变项目方向的取舍（做不做某个功能、删不删某块能力、人设和体验上的改动），由你按用户目标挑最稳妥、可回退的一项直接推进，写进 decisions（每项一句问题、你的选择和理由），同席会把它挂到项目的问答面板（不刷进群聊），用户随时回答；要逐项写出，成员方案里列了几项待定就写几项，question 写清是什么事、choice 写清选了哪个做法，不要合并成「全部按推荐」这种用户看不懂的一条，用户回答了就按回答调整；不要让成员列「待用户拍板」清单，也不要把「集中问用户」排进计划。只有下面几种情况才 action=ask_user 结束并等待真实回答：项目方向上缺少只有用户知道、又猜不出的信息，或用户原话之间互相冲突；要做用户没授权的不可逆或对外的操作（推送、发布、部署或重启生产、删除用户数据、花钱或动用凭据）且没有别的独立工作可做。即使有这类问题，能先做的独立工作照常分派，问题留到无事可做时再问。project_grants 是用户在项目设置里写的长期授权，每次协作都等同用户原话授权，成员也看得到：在它范围内的推送、部署等操作直接派，不用再问。除此之外，成员只认 quote 里本次协作的用户原话作为推送、部署这类操作的授权（以前协作里给的授权、你在 instructions 里写的「已授权」都不算，项目规则也默认不 push）：两处都没有明确授权就不要派这类任务，成员只会核对完不执行；成员交付写明因未授权没有执行时，不要换人或换说法重派同一件事，把它留到最后 ask_user 一次（用户回答后用回答作 quote 再派）。用户纠正方向时，先处理纠正，再启动未开始的任务；仅可用 withdraw 引用新的用户原话取消未完成旧任务，已完成改动不得撤销；用户后续消息改变了任务条件（比如新给了授权）时，可以 withdraw 旧任务、在同一回合用旧任务的同一 source_id/quote 重新派发修订后的说明；正在运行的任务先等待或由用户中断。恢复原任务应保留原 source_id/quote，新的模型授权放在 model_source、纠正内容放在 context_ids。验证结果写明“验证结论：未通过”时，用原实现任务的同一 source_id/quote 再派给原实现成员返修，context_ids 引用该验证结果；返修完成后，改动风险大的才用原验证任务的同一原文再派验证复核（context_ids 引用返修结果），否则以返修成员的自测为准；同一项工作最多返修两轮，仍未通过就 ask_user 询问用户；用户回答同意继续后可再返修两轮，context_ids 同时引用该回答。指定的验证成员已有两次未通过或异常时，或用户要求用改动者的子代理验证时，可派子代理验证：purpose 为 verification、subagent 为 true、agent_id 为最近完成实现的成员，context_ids 引用最新实现结果，model/effort 留空。任务失败不能声称完成；可以 resume 原任务（task_id）保留工作树。仅当任务都完成且目标（分板块推进时是本次的板块）有成员结果支持时 finish，用 result 引用成员交付摘要里的原文；wait 只用于真实失败或被中断的任务等待恢复，否则按计划继续分派或 finish。不要把已完成的成员回合当作仍在运行，也不要让用户额外点击继续。只返回 JSON：{action:'delegate'|'ask_user'|'wait'|'finish'|'pause',ledger?:string,plan?:string,inquiries?:[{key:string,question:string,background?:string,options?:string[],default?:string,reversible?:boolean}],withdraw_inquiries?:string[],routes?:[{key:string,title?:string,task_id?:string,purpose:'solution'|'implementation'|'verification'|'closeout'|'reply',agent_id:string,source_id:string,quote:string,instructions?:string,reason?:string,context_ids?:string[],depends_on?:string[],model?:string|null,effort?:string|null,model_source?:{source_id:string,quote:string},fresh_session?:boolean,clear_context?:boolean,allow_subagents?:boolean,subagent?:boolean,category?:string,complexity?:'low'|'medium'|'high',hold_merge?:boolean,budget_tokens?:number,advisor_calls?:number}],withdraw?:[{task_id:string,source_id:string,quote:string}],decisions?:[{question:string,choice:string,reason?:string}],question?:{prompt:string,options:string[]},result?:{source_id:string,quote:string},next_sections?:[{title:string,summary?:string}],pause?:{reason:string,resume_in_hours?:number}}。依赖可以用本回合之前声明的 route.key 或已有 task_id。协作 finish 后，这次干过活的成员下次都从零上下文开始（隔天缓存早已过期，续接只会整段重读），所以 ledger 和成员交付摘要要写全下次需要的东西。不得输出补丁、凭据、工具调用或额外字段。";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub source_id: String,
    pub quote: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub key: String,
    #[serde(default)]
    pub task_id: Option<String>,
    pub purpose: String,
    pub agent_id: String,
    pub source_id: String,
    pub quote: String,
    #[serde(default)]
    pub context_ids: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub model_source: Option<Reference>,
    /// 子代理验证：交给改动源文件的成员，由它用自带的子代理工具派低价模型、高思考强度的子代理验证。
    #[serde(default)]
    pub subagent: bool,
    /// 调度写的任务说明：目标、产出、资料、边界。与用户原话冲突时以原话为准。
    #[serde(default)]
    pub instructions: Option<String>,
    /// 一句话理由：为什么派这位成员、选这个模型和强度。
    #[serde(default)]
    pub reason: Option<String>,
    /// 新开一个会话（从零上下文开始），之后和用户建的会话一样留在项目里。
    #[serde(default)]
    pub fresh_session: bool,
    /// 成员在原来的会话位置从零上下文开始做这项（旧会话留在原生历史里），不占新开会话的名额。
    #[serde(default)]
    pub clear_context: bool,
    /// 允许成员在本任务里用自带的子 agent。
    #[serde(default)]
    pub allow_subagents: bool,
    /// 任务名：进度里显示，写这项具体做什么。
    #[serde(default)]
    pub title: Option<String>,
    /// 任务类别（capability::CATEGORIES）与复杂度：归档进能力画像，下次选型时按类别看历史表现。
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub complexity: Option<String>,
    /// 合并前须验证：任务完成后先不并入主线，等引用它的验证任务写明通过才合并。
    #[serde(default)]
    pub hold_merge: bool,
    /// 软预算：预计这项的总 token。成员超了先停下交进度报告，由调度决定接着怎么办。
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    /// 请教模式：成员这项任务里最多能向调度请教几次（hub_ask_advisor），不写或 0 就不开放。
    #[serde(default)]
    pub advisor_calls: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub prompt: String,
    #[serde(default)]
    pub options: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub question: String,
    pub choice: String,
    #[serde(default)]
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Withdrawal {
    pub task_id: String,
    pub source_id: String,
    pub quote: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub action: String,
    #[serde(default)]
    pub withdraw: Vec<Withdrawal>,
    #[serde(default)]
    pub routes: Vec<Route>,
    /// 调度自己拍板的取舍：发到群里告知用户，不等回答。
    #[serde(default)]
    pub decisions: Vec<Choice>,
    #[serde(default)]
    pub question: Option<Question>,
    #[serde(default)]
    pub result: Option<Reference>,
    /// 计划账本：目标、步骤与状态、关键决定、风险。下一回合原样交还调度。
    #[serde(default)]
    pub ledger: Option<String>,
    /// 项目长期计划（.tongxi/计划.md）的完整新版；没变就省略。
    #[serde(default)]
    pub plan: Option<String>,
    /// 挂到问答面板的问题（按 key 去重，答过的不再问）。
    #[serde(default)]
    pub inquiries: Vec<crate::project_inquiries::Ask>,
    /// 撤下已经没意义的问题（key）。
    #[serde(default)]
    pub withdraw_inquiries: Vec<String>,
    /// 分板块推进时 finish 带上剩下的板块，发到群里让用户挑哪天做哪块。
    #[serde(default)]
    pub next_sections: Vec<Section>,
    /// 额度或余额撑不下去时主动暂停：说明原因；给了 resume_in_hours 就到点自动继续，不给就停下等用户说继续。
    #[serde(default)]
    pub pause: Option<Pause>,
}
/// 拨款：一项任务用量超了软预算、成员还在干活时，调度只回这一项。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub action: String,
    #[serde(default)]
    pub extra_tokens: Option<u64>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// 请教：成员在任务里问到的关键决定，调度只回这一项。escalate=true 表示建议换更强的模型接手。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Advice {
    pub answer: String,
    #[serde(default)]
    pub escalate: bool,
}

pub fn parse_advice(raw: &str) -> Result<Advice, String> {
    #[derive(Deserialize)]
    struct Reply {
        advice: Advice,
    }
    let document = crate::project_store::json_document(raw, 16_000)?;
    let advice = serde_json::from_str::<Reply>(document).map_err(|_| "请教回复格式无效".to_owned())?.advice;
    if advice.answer.trim().is_empty() {
        return Err("请教回复为空".into());
    }
    Ok(advice)
}

pub fn parse_grant(raw: &str) -> Result<Grant, String> {
    #[derive(Deserialize)]
    struct Reply {
        grant: Grant,
    }
    let document = crate::project_store::json_document(raw, 8_000)?;
    let grant = serde_json::from_str::<Reply>(document).map_err(|_| "拨款回复格式无效".to_owned())?.grant;
    if !matches!(grant.action.as_str(), "continue" | "wrap_up" | "stop") {
        return Err("拨款动作无效".into());
    }
    Ok(grant)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pause {
    pub reason: String,
    #[serde(default)]
    pub resume_in_hours: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Source {
    pub id: String,
    pub kind: String,
    pub text: String,
}

/// 板块标题去掉调度沿用的旧序号（「3. 」「3、」「(3)」「第3块：」），群里统一从 1 重新编号。
pub fn section_title(title: &str) -> &str {
    let text = title.trim();
    let rest = text.strip_prefix('第').unwrap_or(text);
    let rest = rest.strip_prefix(['(', '（']).unwrap_or(rest);
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return text;
    }
    let after = &rest[digits..];
    let after = after.strip_prefix(['块', '项']).unwrap_or(after);
    match after.strip_prefix(['.', '、', ')', '）', ':', '：']) {
        Some(tail) if !tail.trim().is_empty() => tail.trim_start_matches(['.', ' ', ':', '：']).trim(),
        _ => text,
    }
}

pub fn parse(raw: &str) -> Result<Decision, String> {
    let document = crate::project_store::json_document(raw, 64_000)?;
    let value: Decision = serde_json::from_str(document)
        .map_err(|_| "调度返回的 JSON 格式无效或带了未知字段".to_owned())?;
    if !matches!(
        value.action.as_str(),
        "delegate" | "ask_user" | "wait" | "finish" | "pause"
    ) || value.routes.len() > 32
    {
        return Err("管家需返回可执行的路由动作".into());
    }
    if value.withdraw.len() > 32 {
        return Err("每回合撤回任务过多".into());
    }
    if value.decisions.len() > 20
        || value.decisions.iter().any(|item| {
            item.question.trim().is_empty()
                || item.choice.trim().is_empty()
                || [Some(&item.question), Some(&item.choice), item.reason.as_ref()]
                    .into_iter()
                    .flatten()
                    .any(|text| text.chars().count() > 300)
        })
    {
        return Err("decisions 每项写一句问题和你的选择（各300字以内），最多20项".into());
    }
    if value.action == "ask_user" && !value.decisions.is_empty() {
        return Err("提问时不要同时拍板；能自己定的先 delegate 推进".into());
    }
    if value.next_sections.len() > 20
        || value.next_sections.iter().any(|section| {
            section.title.trim().is_empty()
                || section.title.chars().count() > 40
                || section.summary.as_ref().is_some_and(|summary| summary.chars().count() > 200)
        })
    {
        return Err("next_sections 最多 20 项，title 不能为空且不超过40字，summary 不超过200字".into());
    }
    if !value.next_sections.is_empty() && value.action != "finish" {
        return Err("next_sections 只在 finish 时使用".into());
    }
    crate::project_inquiries::check(&value.inquiries)?;
    if value.plan.as_ref().is_some_and(|plan| plan.chars().count() > crate::project_plan::LIMIT) {
        return Err("长期计划过长，请压缩到 12000 字以内，细节放进方案存档按路径引用".into());
    }
    if value.ledger.as_ref().is_some_and(|ledger| ledger.chars().count() > 3000) {
        return Err("计划账本过长，请压缩到1500字以内".into());
    }
    for route in &value.routes {
        if route.instructions.as_ref().is_some_and(|text| text.chars().count() > 4000) {
            return Err("任务说明过长，请控制在2000字以内，细节通过 context_ids 引用".into());
        }
        if route.reason.as_ref().is_some_and(|text| text.chars().count() > 300) {
            return Err("理由只写一句话".into());
        }
        if route.title.as_ref().is_some_and(|text| text.chars().count() > 40) {
            return Err("任务名控制在20字左右".into());
        }
        if route.advisor_calls.is_some_and(|calls| calls > 5) {
            return Err("advisor_calls 每项任务最多 5 次".into());
        }
        if route.fresh_session && route.subagent {
            return Err("子代理验证由改动者本人派出，不能新开会话".into());
        }
    }
    if value.action == "delegate" && value.routes.is_empty() {
        return Err("分派动作没有任务".into());
    }
    if value.action != "delegate" && !value.routes.is_empty() {
        return Err("此动作不能同时追加任务".into());
    }
    if value.action == "ask_user"
        && value.question.as_ref().is_none_or(|q| {
            q.prompt.trim().is_empty()
                || q.prompt.chars().count() > 4000
                || q.options.len() > 8
                || q.options
                    .iter()
                    .any(|s| s.trim().is_empty() || s.chars().count() > 500)
        })
    {
        return Err("请提供清楚的问题与选项".into());
    }
    if value.action != "ask_user" && value.question.is_some() {
        return Err("问题只能通过提问动作发布".into());
    }
    match (&value.pause, value.action == "pause") {
        (None, true) => return Err("暂停要写 pause.reason（为什么暂停），能估出额度何时恢复就给 resume_in_hours".into()),
        (Some(_), false) => return Err("pause 只在 action=pause 时使用".into()),
        (Some(pause), true)
            if pause.reason.trim().is_empty()
                || pause.reason.chars().count() > 300
                || pause.resume_in_hours.is_some_and(|hours| !(hours > 0.0 && hours <= 168.0)) =>
        {
            return Err("pause.reason 写一句原因（300字以内），resume_in_hours 在 0 到 168 之间".into())
        }
        _ => {}
    }
    Ok(value)
}
pub fn source<'a>(
    sources: &'a [Source],
    reference: &Reference,
    user_only: bool,
) -> Result<&'a Source, String> {
    let source = sources
        .iter()
        .find(|s| s.id == reference.source_id)
        .ok_or("引用的原文不存在或属于其他协作")?;
    if user_only && source.kind != "user" {
        return Err("任务范围与模型授权必须来自用户原话".into());
    }
    if reference.quote.trim().is_empty() || !source.text.contains(&reference.quote) {
        return Err("任务引用必须保持原文，不得追加或改写指示".into());
    }
    Ok(source)
}
fn clause<'a>(text: &'a str, quote: &str) -> &'a str {
    let Some(start) = text.find(quote) else {
        return text;
    };
    let end = start + quote.len();
    let left = text[..start]
        .rfind(['，', ',', '。', '；', ';', '\n'])
        .map_or(0, |at| at + text[at..].chars().next().unwrap().len_utf8());
    let right = text[end..]
        .find(['，', ',', '。', '；', ';', '\n'])
        .map_or(text.len(), |at| end + at);
    &text[left..right]
}
#[cfg(test)]
pub fn check_assignment(route: &Route, sources: &[Source]) -> Result<(), String> {
    check_assignment_with(route, sources, &[])
}

/// aliases：用户起的成员名（项目里的成员名、用户建的会话名）→ 底层成员，点名时和「Codex」「Claude」一样算。
pub fn check_assignment_with(route: &Route, sources: &[Source], aliases: &[(String, String)]) -> Result<(), String> {
    if !crate::project_store::is_executor(&route.agent_id)
        || !matches!(
            route.purpose.as_str(),
            "solution" | "implementation" | "verification" | "closeout" | "reply"
        )
    {
        return Err("任务成员或用途无效".into());
    }
    if route.key.is_empty()
        || route.key.len() > 80
        || !route
            .key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("路由 key 无效".into());
    }
    let reference = Reference {
        source_id: route.source_id.clone(),
        quote: route.quote.clone(),
    };
    let user = source(sources, &reference, true)?;
    let text = clause(&user.text, &route.quote).to_lowercase();
    let named = [
        ("codex", "codex-win"),
        ("hermes", "hermes-win"),
        ("dsh", "dsh-win"),
        ("claude", "claude-win"),
    ]
    .into_iter()
    .filter(|(label, _)| text.contains(label))
    .map(|(_, id)| id)
    .chain(
        aliases
            .iter()
            .filter(|(name, _)| !name.is_empty() && text.contains(&name.to_lowercase()))
            .map(|(_, id)| id.as_str()),
    )
    .collect::<Vec<_>>();
    if route.subagent && route.purpose != "verification" {
        return Err("子代理只用于验证".into());
    }
    // 子代理验证由改动者派出，是否允许由 enqueue_routes 按条件判定，这里不按点名拦。
    // 点名按底层成员算：「让 Codex …」可以派给 Codex 的任一席位。
    let base = crate::project_store::base_agent(&route.agent_id);
    if !route.subagent && !named.is_empty() && !named.contains(&base) {
        return Err("这段用户原话已点名另一成员，管家不能换派".into());
    }
    // A task may refer to a predecessor, but must not also include standalone
    // instructions addressed only to other members from the same request.
    for sentence in route.quote.split(['。', '；', ';', '\n']) {
        let sentence = sentence.to_lowercase();
        let actors = [
            ("codex", "codex-win"),
            ("hermes", "hermes-win"),
            ("dsh", "dsh-win"),
            ("claude", "claude-win"),
        ]
        .into_iter()
        .filter(|(label, _)| sentence.contains(label))
        .map(|(_, id)| id)
        .chain(
            aliases
                .iter()
                .filter(|(name, _)| !name.is_empty() && sentence.contains(&name.to_lowercase()))
                .map(|(_, id)| id.as_str()),
        )
        .collect::<Vec<_>>();
        if !route.subagent && !actors.is_empty() && !actors.contains(&base) {
            return Err("任务引用夹带了其他成员的独立分工，请只引用当前成员对应的原文；前置结果通过context_ids传递".into());
        }
    }
    for id in &route.context_ids {
        if !sources.iter().any(|s| s.id == *id) {
            return Err("任务上下文引用不存在".into());
        }
    }
    Ok(())
}
fn normalized(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
fn model_named(model: &ModelOption, text: &str) -> bool {
    let value = normalized(text);
    let id = normalized(&model.id);
    (!id.is_empty() && value.contains(&id))
        || model.name.split(['·', '/']).any(|part| {
            let name = normalized(part);
            name.len() >= 4 && value.contains(&name)
        })
}
/// 给修改者建议的子代理模型：优先成员当前配置的低价档，其次同一提供商下的低价档（haiku/flash/mini/nano/lite），
/// 都没有就沿用成员配置；思考强度取 high（不支持时取该模型最高档）。
pub fn subagent_model(
    models: &[ModelOption],
    fallback: &RoleChoice,
) -> (Option<String>, Option<String>) {
    const CHEAP: [&str; 5] = ["haiku", "flash", "mini", "nano", "lite"];
    let tier = |model: &ModelOption| {
        let text = format!("{} {}", model.id, model.name).to_lowercase();
        CHEAP.iter().position(|word| text.contains(word))
    };
    let configured = fallback
        .model
        .as_ref()
        .and_then(|id| models.iter().find(|model| &model.id == id));
    let provider = configured.and_then(|model| model.provider_id.clone());
    let chosen = configured.filter(|model| tier(model).is_some()).or_else(|| {
        models
            .iter()
            .filter(|model| tier(model).is_some())
            .min_by_key(|model| {
                (
                    provider.is_some() && model.provider_id != provider,
                    model.efforts.is_empty(),
                    tier(model),
                )
            })
    });
    let Some(model) = chosen else {
        return (fallback.model.clone(), Some("high".into()));
    };
    let effort = if model.efforts.iter().any(|effort| effort == "high") {
        Some("high".into())
    } else {
        ["max", "xhigh"]
            .iter()
            .find(|level| model.efforts.iter().any(|effort| effort == *level))
            .map(|level| level.to_string())
    };
    (Some(model.id.clone()), effort)
}
fn provider_score(model: &ModelOption, text: &str) -> usize {
    let value = normalized(text);
    [model.provider_name.as_ref(), model.provider_id.as_ref()]
        .into_iter()
        .flatten()
        .map(|provider| normalized(provider))
        .filter(|name| !name.is_empty() && value.contains(name))
        .map(|name| name.len())
        .max()
        .unwrap_or(0)
}
fn model_family(model: &ModelOption) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(&model.id) {
        if let Some(name) = value
            .as_array()
            .and_then(|array| array.last())
            .and_then(Value::as_str)
        {
            return normalized(name);
        }
    }
    normalized(model.name.split('·').next_back().unwrap_or(&model.name))
}
fn named_effort(text: &str) -> Option<&'static str> {
    let text = text.to_lowercase();
    let mut choices = Vec::new();
    for name in [
        "xhigh", "high", "medium", "low", "minimal", "max", "ultra", "none", "off",
    ] {
        for (at, word) in text.match_indices(name) {
            let before = text[..at].chars().next_back();
            let after = text[at + word.len()..].trim_start().chars().next();
            if !before.is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '\\'))
                && !after.is_some_and(|c| {
                    c.is_ascii_alphanumeric() || matches!(c, '.' | '(' | '/' | '\\')
                })
            {
                choices.push((at, name));
            }
        }
    }
    for (label, value) in [
        ("高强度", "high"),
        ("思考高", "high"),
        ("低强度", "low"),
        ("思考低", "low"),
    ] {
        choices.extend(text.match_indices(label).map(|(at, _)| (at, value)));
    }
    choices
        .into_iter()
        .max_by_key(|(at, _)| *at)
        .map(|(_, name)| name)
}
pub fn overrides(
    route: &Route,
    sources: &[Source],
    models: &[ModelOption],
    fallback: &RoleChoice,
) -> Result<(Option<String>, Option<String>), String> {
    let reference = route.model_source.clone().unwrap_or(Reference {
        source_id: route.source_id.clone(),
        quote: route.quote.clone(),
    });
    let user = source(sources, &reference, true)?;
    let text = clause(&user.text, &reference.quote);
    let named = [
        ("codex", "codex-win"),
        ("hermes", "hermes-win"),
        ("dsh", "dsh-win"),
        ("claude", "claude-win"),
    ]
    .into_iter()
    .filter(|(label, _)| text.to_lowercase().contains(label))
    .map(|(_, id)| id)
    .collect::<Vec<_>>();
    // 只有用户原话里的点名才算授权；调度自己按任务选的模型/强度不在此列。
    let parameter_authority = route.model_source.is_some()
        || models.iter().any(|model| model_named(model, text))
        || (named_effort(text).is_some()
            && ["思考", "推理", "reasoning", "effort"]
                .iter()
                .any(|label| text.to_lowercase().contains(label)));
    if parameter_authority && !named.is_empty() && (named.len() != 1 || named[0] != route.agent_id)
    {
        return Err("模型与强度授权应对应当前成员，不能取用另一成员的参数".into());
    }
    if route.model_source.is_some()
        && named.is_empty()
        && !text.contains("所有")
        && !text.to_lowercase().contains("all ")
    {
        let scope = source(
            sources,
            &Reference {
                source_id: route.source_id.clone(),
                quote: route.quote.clone(),
            },
            true,
        )?;
        if !clause(&scope.text, &route.quote).contains(&reference.quote) {
            return Err("跨句模型授权需明确对应当前成员".into());
        }
    }
    let matches = models
        .iter()
        .filter(|model| model_named(model, text))
        .collect::<Vec<_>>();
    let provider_score_max = matches
        .iter()
        .map(|model| provider_score(model, text))
        .max()
        .unwrap_or(0);
    let chosen = if let Some(id) = &route.model {
        let model = models
            .iter()
            .find(|model| model.id == *id)
            .ok_or("这个模型不在该成员的模型目录里")?;
        // 用户点名了模型就以用户为准；没点名时调度可以按任务自选。
        if !matches.is_empty() && !model_named(model, text) {
            return Err("用户原话点名了模型，以用户为准".into());
        }
        let alternative_provider = matches.iter().any(|other| {
            other.provider_id.is_some()
                && other.provider_id != model.provider_id
                && model_family(other) == model_family(model)
        });
        if alternative_provider
            && ((provider_score_max > 0 && provider_score(model, text) < provider_score_max)
                || (provider_score_max == 0 && fallback.model.as_ref() != Some(id)))
        {
            return Err("用户没有点名另一条提供商线路，应沿用项目配置或问清".into());
        }
        Some(model)
    } else if matches.len() == 1 {
        matches.first().copied()
    } else {
        let named_providers = matches
            .iter()
            .filter(|model| {
                provider_score_max > 0 && provider_score(model, text) == provider_score_max
            })
            .copied()
            .collect::<Vec<_>>();
        if named_providers.len() == 1 {
            named_providers.first().copied()
        } else if matches
            .windows(2)
            .all(|pair| model_family(pair[0]) == model_family(pair[1]))
        {
            matches
                .iter()
                .find(|model| Some(&model.id) == fallback.model.as_ref())
                .copied()
        } else {
            return Err("原文提到多个型号，请通过model字段明确用户要求的目标型号，不能猜选".into());
        }
    };
    if matches.len() > 1 && chosen.is_none() {
        return Err("该型号有多条提供商线路，需要保留当前线路或向用户问清".into());
    }
    let effort_context = !matches.is_empty()
        || ["思考", "推理", "reasoning", "effort"]
            .iter()
            .any(|label| text.to_lowercase().contains(label));
    let named = effort_context
        .then(|| named_effort(text))
        .flatten()
        .map(str::to_owned);
    // 用户点名了强度就以用户为准；没点名时用调度按任务选的（可为空＝沿用项目配置）。
    let effort = match (named, &route.effort) {
        (Some(user), Some(chosen)) if *chosen != user => {
            return Err("用户原话点名了思考强度，以用户为准".into())
        }
        (Some(user), _) => Some(user),
        (None, chosen) => chosen.clone(),
    };
    let model = chosen.map(|model| model.id.clone());
    let effective = model.as_deref().or(fallback.model.as_deref());
    crate::models::validate_selection(
        models,
        effective,
        effort.as_deref().or(fallback.effort.as_deref()),
        fallback.model.as_deref(),
    )?;
    Ok((model, effort))
}
/// 成员失败是否因为模型额度用完或被限流：这类失败等额度恢复后重试即可，不该让整轮协作中断。
pub fn quota_exhausted(error: &str) -> bool {
    let text = error.to_lowercase();
    [
        "限流",
        "额度",
        "rate limit",
        "rate_limit",
        "ratelimit",
        "usage limit",
        "quota",
        "too many requests",
        "429",
    ]
    .iter()
    .any(|word| text.contains(word))
}
/// 额度等待的重试间隔（分钟）：5、10、15……封顶 30。
pub fn quota_retry_minutes(waits: u32) -> u64 {
    (5 * (u64::from(waits) + 1)).min(30)
}
/// 子代理验证的任务说明：由修改者本人用自带的子代理工具派低价模型、高思考强度的子代理独立验证。
pub fn subagent_scope(agent: &str, cheap: Option<&str>) -> String {
    let tool = match agent {
        "claude-win" => format!(
            "用 Agent 工具派子代理，model 设为 {}、effort 设为 high",
            cheap.unwrap_or("haiku")
        ),
        _ => format!(
            "用你自带的子代理/委派工具派子代理，选低价模型{}、高思考强度",
            cheap.map(|model| format!("（如 {model}）")).unwrap_or_default()
        ),
    };
    format!(
        "本次是子代理验证：你是这些改动的作者，不要自己验证，{tool}。子代理的任务说明里写清用户原话、被验证的改动与验证范围，要求它独立验证、不修改被验证的源文件（需要时可写验证脚本），并在最后写“验证结论：通过”或“验证结论：未通过”及问题清单。assigned_excerpt 若点名了其他验证成员，本次由你的子代理代为验证。拿到结果后如实转述，不改写结论；你的最后一行照抄子代理的验证结论。若你没有子代理工具，说明原因并写“验证结论：未通过”，不要自己代验。"
    )
}
/// 成员交代这项超出能力、要换强模型接手：交付里单独一行「需要升档：原因」，返回原因。
pub fn escalation(output: &str) -> Option<&str> {
    output.lines().rev().find_map(|line| {
        let line = line.trim().trim_start_matches(['*', '-', ' ']);
        line.strip_prefix("需要升档：").or_else(|| line.strip_prefix("需要升档:")).map(str::trim)
    })
}

/// 成员交付摘要的开头；调度只读这一段。
pub const DELIVERY_MARK: &str = "交付摘要：";

/// 调度看到的成员结果：有交付摘要就只给摘要，没有就留头尾各一段，省得每轮重发全文。
pub fn delivery_summary(output: &str) -> String {
    if let Some(at) = output.rfind(DELIVERY_MARK) {
        return output[at..].chars().take(2400).collect();
    }
    let chars = output.chars().collect::<Vec<_>>();
    if chars.len() <= 3000 {
        return output.to_owned();
    }
    format!(
        "{}\n……（中间省略 {} 字）……\n{}",
        chars[..1200].iter().collect::<String>(),
        chars.len() - 2400,
        chars[chars.len() - 1200..].iter().collect::<String>()
    )
}

pub fn member_prompt(
    agent: &str,
    purpose: &str,
    user_request: &str,
    quote: &str,
    contexts: Vec<Value>,
    brief: Option<&str>,
    allow_subagents: bool,
) -> Value {
    let scope = match purpose {
        "solution" => "本次只完成用户要求的方案或分析，不提前执行引用中的后续实现、验证或收尾。",
        "implementation" => "本次完成被分配的实现范围，不接取引用中分给其他成员的工作。若 context_sources 里有验证成员报告的问题，本次就是返修：逐条修复这些问题，并说明每条如何处理。",
        "verification" => "本次完成被分配的验证范围，按用户原话决定是否需要编写验证脚本。最后单独一行写“验证结论：通过”或“验证结论：未通过”；未通过时列出问题清单（文件、现象、复现方式），供管家安排返修。",
        "closeout" => "本次完成用户要求的整理或收尾，报告实际完成的内容。",
        _ => "本次回复当前问题，不提前执行引用中的其他工作。",
    };
    let mut prompt = serde_json::json!({
      "instruction":"按用户原话、当前任务原文和调度的任务说明完成工作。user_request 是目标来源，assigned_excerpt 是本次分工范围，coordinator_brief 是调度写的任务说明（目标、产出、资料、边界），context_sources 是已有方案或成员结果；不要执行其他成员的职责。coordinator_brief 与用户原话冲突时以用户原话为准，并在回复里说明。若路由成员、任务用途或方案内容与用户明确要求冲突，向用户提问，不擅自改范围或换人。任务里遇到要取舍的地方，在任务边界内选最稳妥、可回退的做法直接做，在摘要里写明你选了什么；不可逆或对外的操作（推送、发布、部署或重启生产、删除数据）没有授权就不做，写在摘要的遗留问题里交给调度，不要列「待用户拍板」清单，也不要停下来等用户。需要提交、推送或发布时，以当前用户明确要求为授权，不能把成员建议当作授权；project_grants 是用户在项目设置里写的长期授权，在它范围内的操作同样算用户明确授权，照做即可。保留已完成改动，报告实际结果与未解决的问题。这项超出你当前模型的能力、试过几种办法仍做不好时（不是额度、网络、线路这类环境问题），不要硬撑或凑合交差：保留已做的改动，在交付摘要里单独一行写「需要升档：卡在哪、试过什么」，调度会换更强的模型在你的工作树上接手。可用原生工具工作、向成员沟通；省着用上下文：大文件和长输出按需读片段（grep、head、按行号范围），不要整份读入，也不要反复读同一内容。实在无法继续时才把问题发到团队等待真实回答。由谁做、在哪个会话、用什么模型和思考强度，调度已经按用户要求安排好：你就在当前会话和模型里工作，原话里新开会话、换模型这类安排不用你执行，也不必在回复里解释。方案、实现、验证、收尾任务在回复最后写一段以「交付摘要：」开头、不超过300字的摘要：做了什么、改了哪些文件、验证情况、遗留问题；调度只读这段摘要。任务要求写验证结论时，验证结论那一行放在摘要之后的最后一行。简短回复不需要摘要；用户指定了回复格式（如只回复数字）时完全按用户的格式。",
      "assigned_agent":agent,"purpose":purpose,"task_scope":scope,"user_request":user_request,"assigned_excerpt":quote,"context_sources":contexts
    });
    if let Some(brief) = brief.map(str::trim).filter(|brief| !brief.is_empty()) {
        prompt["coordinator_brief"] = Value::from(brief);
    }
    if allow_subagents {
        prompt["subagents"] = Value::from("本任务允许用你自带的子 agent 工具拆分并行调研或做独立验证；子 agent 尽量选低价模型，把子 agent 的结论整理进你的回复。");
    }
    prompt
}
