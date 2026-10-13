import { invoke } from '@tauri-apps/api/core';
import type { ConversationDetail, Roles, RoleChoice, RuntimeSnapshot } from './types';
import { modelOptionsHtml, modelProviders, providerOptionsHtml, selectedProvider } from './model_select';

type Escape=(value:string|number)=>string;
const effortNames:Record<string,string>={none:'关闭',off:'关闭',low:'低',medium:'中',high:'高',xhigh:'很高',max:'最大',ultra:'Ultra'};
interface Host { escape:Escape; modal:HTMLDialogElement; open:(title:string,note:string,body:string)=>void; snapshot:(id:string)=>RuntimeSnapshot; name:(id:string)=>string; saved:()=>Promise<void>; }

export async function projectSettings(detail:ConversationDetail, host:Host) {
  const {escape}=host;
  const chats=await Promise.all(detail.project_agent_chats.map(chat=>invoke<ConversationDetail>('get_conversation',{id:chat.conversation_id})));
  const ids=chats.map(chat=>chat.conversation.members[0]);
  const available=(saved:string|undefined,preference:string[])=>saved&&ids.includes(saved)?saved:preference.find(id=>ids.includes(id))||ids[0];
  const current=detail.project_roles;
  const initial={
    plan:available(current?.plan.agent,['codex-win','hermes-win','claude-win','dsh-win']),
    implement:available(current?.implement.agent,['dsh-win','codex-win','claude-win','hermes-win']),
    review:available(current?.review.agent,['hermes-win','codex-win','claude-win','dsh-win']),
  };
  const coordinator:RoleChoice={agent:available(detail.project_coordinator?.agent,['hermes-win','codex-win','dsh-win','claude-win']),model:detail.project_coordinator?.model||null,effort:detail.project_coordinator?.effort||null};
  host.open('项目成员配置','管家按原话协调任务，方案与实际工作交给项目成员。',`<form id="project-member-settings"><fieldset class="project-coordinator-settings"><legend>调度管家</legend><label>协调需求与分工<select data-project-coordinator aria-label="调度管家">${ids.map(id=>`<option value="${escape(id)}" ${coordinator.agent===id?'selected':''}>${escape(host.name(id))}</option>`).join('')}</select></label><div data-coordinator-model-controls></div><small>管家使用隐藏的独立会话，只调度，不给成员追加业务指示。配置影响接下来启动的协作。</small></fieldset><fieldset class="project-planner-settings"><legend>默认方案者</legend><label>未点名时由谁出方案<select data-project-planner aria-label="默认方案者">${ids.map(id=>`<option value="${escape(id)}" ${initial.plan===id?'selected':''}>${escape(host.name(id))}</option>`).join('')}</select></label><small>你在消息里指定的成员、模型和任务范围优先。</small></fieldset>${chats.map(chat=>{
    const agent=chat.conversation.members[0],runtime=host.snapshot(agent),settings=chat.sessions[0];
    const model=settings.model || runtime.default_model || '';
    const provider=selectedProvider(runtime.models,model);
    const efforts=runtime.models.find(option=>option.id===model)?.efforts || [];
    const unavailable=model && !runtime.models.some(option=>option.id===model) ? `<option value="${escape(model)}" selected>${escape(model)} · 待连接核对</option>` : '';
    return `<fieldset class="project-member-settings-row" data-settings-chat="${escape(chat.conversation.id)}" data-settings-agent="${escape(agent)}"><legend>${escape(host.name(agent))}</legend>${modelProviders(runtime.models).length>1?`<label>提供商<select data-member-provider>${providerOptionsHtml(runtime.models,provider,escape)}</select></label>`:''}<label>模型<select data-member-model>${modelOptionsHtml(runtime.models,provider,model||null,escape,'连接后选择模型')}${unavailable}</select></label><label>思考强度<select data-member-effort><option value="">模型默认</option>${efforts.map(value=>`<option value="${escape(value)}" ${value===settings.reasoning_effort?'selected':''}>${escape(effortNames[value]||value)}</option>`).join('')}</select></label><small>${runtime.connection==='connected'?'已连接':'请先连接此成员读取模型目录'}</small></fieldset>`;
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
  form.addEventListener('submit',async event=>{
    event.preventDefault();const button=form.querySelector<HTMLButtonElement>('button.primary')!;button.disabled=true;
    try {
      const choices=[...form.querySelectorAll<HTMLElement>('[data-settings-agent]')].map(row=>({id:row.dataset.settingsChat!,agentId:row.dataset.settingsAgent!,model:row.querySelector<HTMLSelectElement>('[data-member-model]')!.value,reasoningEffort:row.querySelector<HTMLSelectElement>('[data-member-effort]')!.value||null}));
      if(choices.some(choice=>!choice.model||host.snapshot(choice.agentId).connection!=='connected'))throw new Error('请先连接所有项目成员，并为每位成员选择模型');
      for(const choice of choices)await invoke('set_session_settings',choice);
      const roles:Roles={plan:{agent:form.querySelector<HTMLSelectElement>('[data-project-planner]')!.value,model:null,effort:null},implement:{agent:initial.implement,model:null,effort:null},review:{agent:initial.review,model:null,effort:null}};
      for(const role of [roles.plan,roles.implement,roles.review]) {
        const selected=choices.find(choice=>choice.agentId===role.agent);
        role.model=selected?.model||null;role.effort=selected?.reasoningEffort||null;
      }
      await invoke('set_project_roles',{conversationId:detail.conversation.id,roles});
      await invoke('set_project_coordinator',{conversationId:detail.conversation.id,coordinator});
      host.modal.close();await host.saved();
    } catch(error){form.querySelector('.form-error')!.textContent=String(error);button.disabled=false;}
  });
}
