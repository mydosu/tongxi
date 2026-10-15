import { invoke } from '@tauri-apps/api/core';
import type { ConversationDetail, Roles, RoleChoice, RuntimeSnapshot } from './types';
import { modelOptionsHtml, modelProviders, providerOptionsHtml, selectedProvider } from './model_select';
import { isSeat } from './seats';

type Escape=(value:string|number)=>string;
const effortNames:Record<string,string>={none:'关闭',off:'关闭',low:'低',medium:'中',high:'高',xhigh:'很高',max:'最大',ultra:'Ultra'};
interface Host { escape:Escape; modal:HTMLDialogElement; open:(title:string,note:string,body:string)=>void; snapshot:(id:string)=>RuntimeSnapshot; name:(id:string)=>string; saved:()=>Promise<void>; reopen:()=>Promise<void>; renamed:(seat:string,label:string)=>void; globalName:(id:string)=>string; busy:boolean; toast:(text:string,error?:boolean)=>void; avatar:(id:string)=>string; }
const PROJECT_AGENTS=['codex-win','hermes-win','dsh-win','claude-win'];
const PREFERENCES:[string,string][]=[['save','省额度：能省则省'],['balanced','平衡'],['quality','质量优先']];

export async function projectSettings(detail:ConversationDetail, host:Host) {
  const {escape}=host;
  const chats=await Promise.all(detail.project_agent_chats.map(chat=>invoke<ConversationDetail>('get_conversation',{id:chat.conversation_id})));
  // 每个席位一行：seat 是席位 id（主席位就是成员 id），模型设置写在该席位自己的专属会话上。
  const seatOf=(chat:ConversationDetail)=>detail.project_agent_chats.find(item=>item.conversation_id===chat.conversation.id)?.agent_id||chat.conversation.members[0];
  const ids=chats.map(chat=>chat.conversation.members[0]).filter((id,index,all)=>all.indexOf(id)===index);
  const available=(saved:string|undefined,preference:string[])=>saved&&ids.includes(saved)?saved:preference.find(id=>ids.includes(id))||ids[0];
  const current=detail.project_roles;
  const initial={
    plan:available(current?.plan.agent,['codex-win','hermes-win','claude-win','dsh-win']),
    implement:available(current?.implement.agent,['dsh-win','codex-win','claude-win','hermes-win']),
    review:available(current?.review.agent,['hermes-win','codex-win','claude-win','dsh-win']),
  };
  const coordinator:RoleChoice={agent:available(detail.project_coordinator?.agent,['hermes-win','codex-win','dsh-win','claude-win']),model:detail.project_coordinator?.model||null,effort:detail.project_coordinator?.effort||null};
  host.open('项目成员配置','调度按你的原话拆计划、派任务、挑模型，方案与实际工作交给项目成员。',`<form id="project-member-settings"><fieldset class="project-roster"><legend>项目成员</legend><div class="roster-options">${PROJECT_AGENTS.map(id=>`<label class="roster-option"><input type="checkbox" data-roster value="${escape(id)}" ${detail.conversation.members.includes(id)?'checked':''} ${host.busy?'disabled':''}>${host.avatar(id)}<span>${escape(host.globalName(id))}</span></label>`).join('')}<button type="button" class="secondary" data-roster-save ${host.busy?'disabled':''}>更新成员</button></div><small>${host.busy?'有协作正在进行，结束或停止后才能调整成员。':'勾选参加这个项目的成员（至少一位；同一成员多开的会话也算成员，一位成员开两个会话就能协作）。新加入的成员会自动建好专属会话；移出的成员会话和历史都保留，加回来就恢复。'}</small></fieldset><fieldset class="project-coordinator-settings"><legend>调度管家</legend><label>协调需求与分工<select data-project-coordinator aria-label="调度管家">${ids.map(id=>`<option value="${escape(id)}" ${coordinator.agent===id?'selected':''}>${escape(host.globalName(id))}</option>`).join('')}</select></label><div data-coordinator-model-controls></div><label>调度倾向<select data-project-preference aria-label="调度倾向">${PREFERENCES.map(([value,label])=>`<option value="${value}" ${(detail.project_preference||'balanced')===value?'selected':''}>${label}</option>`).join('')}</select></label><small>调度是主脑：拆计划、给成员写任务说明、按任务挑模型和思考强度，必要时新开临时会话（省额度最多 1 个、平衡 2 个、质量优先 3 个，协作结束自动撤掉）。它会参考各家计费规则和会话上下文大小；你在消息里点名的成员、模型和强度一律优先。管家用所选成员另开的隐藏会话，建议给它配强模型。配置影响接下来启动的协作。</small></fieldset><fieldset class="project-planner-settings"><legend>默认方案者</legend><label>未点名时由谁出方案<select data-project-planner aria-label="默认方案者">${ids.map(id=>`<option value="${escape(id)}" ${initial.plan===id?'selected':''}>${escape(host.name(id))}</option>`).join('')}</select></label><small>你在消息里指定的成员、模型和任务范围优先。</small></fieldset><fieldset class="project-seat-settings"><legend>同一成员多开会话</legend><div class="seat-add-row"><select data-seat-agent aria-label="成员">${ids.map(id=>`<option value="${escape(id)}">${escape(host.globalName(id))}</option>`).join('')}</select><input data-seat-label maxlength="20" placeholder="新会话的名字（可选，如：快手、审稿）"><button type="button" class="secondary" data-seat-add>再开一个会话</button></div><small>同一成员最多开 4 个会话，彼此平行、不分主次：每个都是独立的原生会话，模型可以相同也可以不同，调度把它们当作不同成员分工。名字只在本项目里生效，成员本身的名字在「成员与服务」里改。</small></fieldset>${chats.map(chat=>{
    const agent=chat.conversation.members[0],seat=seatOf(chat),runtime=host.snapshot(agent),settings=chat.sessions[0];
    const model=settings.model || runtime.default_model || '';
    const provider=selectedProvider(runtime.models,model);
    const efforts=runtime.models.find(option=>option.id===model)?.efforts || [];
    const unavailable=model && !runtime.models.some(option=>option.id===model) ? `<option value="${escape(model)}" selected>${escape(model)} · 待连接核对</option>` : '';
    return `<fieldset class="project-member-settings-row seat-row" data-settings-chat="${escape(chat.conversation.id)}" data-settings-agent="${escape(agent)}" data-settings-seat="${escape(seat)}"><legend><input class="seat-name" data-seat-rename="${escape(seat)}" value="${escape(detail.project_agent_chats.find(item=>item.agent_id===seat)?.label||'')}" placeholder="${escape(host.name(seat))}" maxlength="20" aria-label="会话名" title="点一下起名，回车或点开就保存；清空恢复默认"><span class="seat-of">${escape(host.globalName(agent))} 的会话</span>${isSeat(seat)?`<button type="button" class="text-button" data-seat-remove="${escape(seat)}">撤掉</button>`:''}</legend>${modelProviders(runtime.models).length>1?`<label>提供商<select data-member-provider>${providerOptionsHtml(runtime.models,provider,escape)}</select></label>`:''}<label>模型<select data-member-model>${modelOptionsHtml(runtime.models,provider,model||null,escape,'连接后选择模型')}${unavailable}</select></label><label>思考强度<select data-member-effort><option value="">模型默认</option>${efforts.map(value=>`<option value="${escape(value)}" ${value===settings.reasoning_effort?'selected':''}>${escape(effortNames[value]||value)}</option>`).join('')}</select></label><small>${runtime.connection==='connected'?'已连接':'请先连接此成员读取模型目录'}</small></fieldset>`;
  }).join('')}<p class="form-error"></p><div class="modal-footer"><button type="button" id="project-settings-cancel" class="secondary">取消</button><button class="primary">保存项目配置</button></div></form>`);
  const form=host.modal.querySelector<HTMLFormElement>('#project-member-settings')!;
  const renderCoordinator=()=>{
    const runtime=host.snapshot(coordinator.agent),settings=chats.find(chat=>chat.conversation.members[0]===coordinator.agent)?.sessions[0];
    const selected=coordinator.model||settings?.model||runtime.default_model||'';
    const provider=selectedProvider(runtime.models,selected);
    const models=runtime.models;
    const efforts=models.find(model=>model.id===selected)?.efforts||[];
    form.querySelector('[data-coordinator-model-controls]')!.innerHTML=`${modelProviders(models).length>1?`<label>提供商<select data-coordinator-provider>${providerOptionsHtml(models,provider,escape)}</select></label>`:''}<label>管家模型<select data-coordinator-model>${modelOptionsHtml(models,provider,coordinator.model,escape,'沿用该成员项目配置')}</select></label><label>管家思考强度<select data-coordinator-effort><option value="">沿用该成员项目配置</option>${efforts.map(value=>`<option value="${escape(value)}" ${coordinator.effort===value?'selected':''}>${escape(effortNames[value]||value)}</option>`).join('')}</select></label>`;
    form.querySelector<HTMLSelectElement>('[data-coordinator-provider]')?.addEventListener('change',event=>{
      coordinator.model=modelProviders(models).find(provider=>provider.id===(event.target as HTMLSelectElement).value)?.models[0]?.id||null;coordinator.effort=null;renderCoordinator();
    });
    form.querySelector<HTMLSelectElement>('[data-coordinator-model]')!.addEventListener('change',event=>{coordinator.model=(event.target as HTMLSelectElement).value||null;coordinator.effort=null;renderCoordinator();});
    form.querySelector<HTMLSelectElement>('[data-coordinator-effort]')!.addEventListener('change',event=>{coordinator.effort=(event.target as HTMLSelectElement).value||null;});
  };
  form.querySelector<HTMLSelectElement>('[data-project-coordinator]')!.addEventListener('change',event=>{coordinator.agent=(event.target as HTMLSelectElement).value;coordinator.model=null;coordinator.effort=null;renderCoordinator();});
  renderCoordinator();
  for (const row of form.querySelectorAll<HTMLElement>('[data-settings-agent]')) {
    const models=host.snapshot(row.dataset.settingsAgent!).models;
    const model=row.querySelector<HTMLSelectElement>('[data-member-model]')!;
    const refreshEfforts=()=>{row.querySelector<HTMLSelectElement>('[data-member-effort]')!.innerHTML='<option value="">模型默认</option>'+(models.find(item=>item.id===model.value)?.efforts||[]).map(value=>`<option value="${escape(value)}">${escape(effortNames[value]||value)}</option>`).join('');};
    row.querySelector<HTMLSelectElement>('[data-member-provider]')?.addEventListener('change',event=>{
      const provider=(event.target as HTMLSelectElement).value,first=modelProviders(models).find(item=>item.id===provider)?.models[0];
      model.innerHTML=modelOptionsHtml(models,provider,first?.id||null,escape);refreshEfforts();
    });
    model.addEventListener('change',refreshEfforts);
  }
  form.querySelector('#project-settings-cancel')!.addEventListener('click',()=>host.modal.close());
  // 增删席位立即生效（不等「保存」），之后重开这个弹窗显示最新的席位。
  const seatAction=async(work:()=>Promise<unknown>)=>{
    form.querySelectorAll<HTMLButtonElement>('button').forEach(button=>{button.disabled=true;});
    // 不关弹窗：后台刷新后原地换上新内容（openModal 在弹窗开着时只换内容、保留滚动位置）。
    try{await work();await host.reopen();}
    catch(error){form.querySelector('.form-error')!.textContent=String(error);form.querySelectorAll<HTMLButtonElement>('button').forEach(button=>{button.disabled=false;});}
  };
  // 增减项目成员：立即生效（协作运行中锁住）；移出的若是调度管家，会回到默认管家。
  form.querySelector('[data-roster-save]')!.addEventListener('click',()=>{
    const chosen=[...form.querySelectorAll<HTMLInputElement>('[data-roster]:checked')].map(input=>input.value);
    if(chosen.length<1){form.querySelector('.form-error')!.textContent='项目至少需要一位成员';return;}
    const others=detail.conversation.members.filter(id=>!PROJECT_AGENTS.includes(id));
    const dropped=detail.conversation.members.filter(id=>PROJECT_AGENTS.includes(id)&&!chosen.includes(id));
    void seatAction(async()=>{
      await invoke('update_group_members',{id:detail.conversation.id,members:[...chosen,...others]});
      if(dropped.includes(coordinator.agent))host.toast(`调度管家 ${host.globalName(coordinator.agent)} 已移出，已换回默认管家`);
      else host.toast('项目成员已更新');
    });
  });
  form.querySelector('[data-seat-add]')!.addEventListener('click',()=>void seatAction(()=>invoke('add_project_seat',{conversationId:detail.conversation.id,agentId:form.querySelector<HTMLSelectElement>('[data-seat-agent]')!.value,label:form.querySelector<HTMLInputElement>('[data-seat-label]')!.value.trim()||null})));
  // 席位改名：失焦或回车就保存，不用等「保存项目配置」。
  form.querySelectorAll<HTMLInputElement>('[data-seat-rename]').forEach(input=>{
    // 空着＝用默认名（占位符显示默认名）；清空后保存＝恢复默认。
    let last=input.value;
    const save=async()=>{
      const label=input.value.trim();
      if(label===last)return;
      try{const saved=await invoke<string>('rename_project_seat',{conversationId:detail.conversation.id,seatId:input.dataset.seatRename,label});if(!label)input.placeholder=saved;last=input.value=label;host.renamed(input.dataset.seatRename!,saved);}
      catch(error){form.querySelector('.form-error')!.textContent=String(error);input.value=last;}
    };
    input.addEventListener('blur',()=>void save());
    input.addEventListener('keydown',event=>{if(event.key==='Enter'){event.preventDefault();input.blur();}if(event.key==='Escape'){event.stopPropagation();input.value=last;input.blur();}});
  });
  form.querySelectorAll<HTMLButtonElement>('[data-seat-remove]').forEach(button=>button.addEventListener('click',()=>void seatAction(()=>invoke('remove_project_seat',{conversationId:detail.conversation.id,seatId:button.dataset.seatRemove}))));
  form.addEventListener('submit',async event=>{
    event.preventDefault();const button=form.querySelector<HTMLButtonElement>('button.primary')!;button.disabled=true;
    try {
      const choices=[...form.querySelectorAll<HTMLElement>('[data-settings-agent]')].map(row=>({id:row.dataset.settingsChat!,agentId:row.dataset.settingsAgent!,seat:row.dataset.settingsSeat!,model:row.querySelector<HTMLSelectElement>('[data-member-model]')!.value,reasoningEffort:row.querySelector<HTMLSelectElement>('[data-member-effort]')!.value||null}));
      if(choices.some(choice=>!choice.model||host.snapshot(choice.agentId).connection!=='connected'))throw new Error('请先连接所有项目成员，并为每位成员选择模型');
      for(const {seat:_seat,...choice} of choices)await invoke('set_session_settings',choice);
      const roles:Roles={plan:{agent:form.querySelector<HTMLSelectElement>('[data-project-planner]')!.value,model:null,effort:null},implement:{agent:initial.implement,model:null,effort:null},review:{agent:initial.review,model:null,effort:null}};
      for(const role of [roles.plan,roles.implement,roles.review]) {
        const selected=choices.find(choice=>choice.seat===role.agent);
        role.model=selected?.model||null;role.effort=selected?.reasoningEffort||null;
      }
      await invoke('set_project_roles',{conversationId:detail.conversation.id,roles});
      await invoke('set_project_coordinator',{conversationId:detail.conversation.id,coordinator});
      await invoke('set_project_preference',{conversationId:detail.conversation.id,preference:form.querySelector<HTMLSelectElement>('[data-project-preference]')!.value});
      host.modal.close();await host.saved();
    } catch(error){form.querySelector('.form-error')!.textContent=String(error);button.disabled=false;}
  });
}
