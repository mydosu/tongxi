import { invoke } from '@tauri-apps/api/core';
import type { ConversationDetail, RuntimeSnapshot } from './types';
import { modelOptionsHtml, modelProviders, providerOptionsHtml, selectedProvider } from './model_select';

type Escape=(value:string|number)=>string;
const effortNames:Record<string,string>={none:'关闭',off:'关闭',low:'低',medium:'中',high:'高',xhigh:'很高',max:'最大',ultra:'Ultra'};
interface Host { escape:Escape; modal:HTMLDialogElement; open:(title:string,note:string,body:string)=>void; snapshot:(id:string)=>RuntimeSnapshot; name:(id:string)=>string; saved:()=>Promise<void>; }

export async function projectSettings(detail:ConversationDetail, host:Host) {
  const {escape}=host;
  const chats=await Promise.all(detail.project_agent_chats.map(chat=>invoke<ConversationDetail>('get_conversation',{id:chat.conversation_id})));
  host.open('项目成员配置','为每位成员保存本项目使用的提供商、模型和思考强度。任务分配只指定成员与职责。',`<form id="project-member-settings">${chats.map(chat=>{
    const agent=chat.conversation.members[0],runtime=host.snapshot(agent),settings=chat.sessions[0];
    const model=settings.model || runtime.default_model || '';
    const provider=selectedProvider(runtime.models,model);
    const efforts=runtime.models.find(option=>option.id===model)?.efforts || [];
    const unavailable=model && !runtime.models.some(option=>option.id===model) ? `<option value="${escape(model)}" selected>${escape(model)} · 待连接核对</option>` : '';
    return `<fieldset class="project-member-settings-row" data-settings-chat="${escape(chat.conversation.id)}" data-settings-agent="${escape(agent)}"><legend>${escape(host.name(agent))}</legend>${modelProviders(runtime.models).length>1?`<label>提供商<select data-member-provider>${providerOptionsHtml(runtime.models,provider,escape)}</select></label>`:''}<label>模型<select data-member-model>${modelOptionsHtml(runtime.models,provider,model||null,escape,'连接后选择模型')}${unavailable}</select></label><label>思考强度<select data-member-effort><option value="">模型默认</option>${efforts.map(value=>`<option value="${escape(value)}" ${value===settings.reasoning_effort?'selected':''}>${escape(effortNames[value]||value)}</option>`).join('')}</select></label><small>${runtime.connection==='connected'?'已连接':'请先连接此成员读取模型目录'}</small></fieldset>`;
  }).join('')}<p class="form-error"></p><div class="modal-footer"><button type="button" id="project-settings-cancel" class="secondary">取消</button><button class="primary">保存项目配置</button></div></form>`);
  const form=host.modal.querySelector<HTMLFormElement>('#project-member-settings')!;
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
      host.modal.close();await host.saved();
    } catch(error){form.querySelector('.form-error')!.textContent=String(error);button.disabled=false;}
  });
}
