import { invoke } from '@tauri-apps/api/core';
import type { ChatQuestion } from './types';

type Escape=(value:string|number)=>string;
export function questionCard(question:ChatQuestion,escape:Escape) {
  const pending=question.status==='pending';
  return `<section class="chat-question-card" data-chat-question="${escape(question.id)}"><header><span class="question-dot"></span><strong>${pending?'等待你的回答':question.status==='cancelled'?'问题已取消':question.delivery==='queued'?'回答已保存，等待成员继续':'已回答'}</strong></header><p>${escape(question.prompt)}</p>${question.error?`<small class="workflow-error">${escape(question.error)}</small>`:''}${pending?`<div class="chat-question-actions"><button type="button" class="secondary" data-answer-chat-question="${escape(question.id)}" data-question-room="${escape(question.conversation_id)}">回答</button>${question.mode==='deferred'?`<button type="button" class="text-button" data-cancel-chat-question="${escape(question.id)}" data-question-room="${escape(question.conversation_id)}">取消等待</button>`:''}</div>`:question.answer?`<small class="chat-question-answered">你的回答：${escape(question.answer)}</small>`:''}</section>`;
}

export function visibleQuestionReply(content:string,streaming=false) {
  const opening='```tongxi-question';
  return content.replace(/```tongxi-question\s*([\s\S]*?)```/g,(block,json)=>{
    try { const value=JSON.parse(json);return typeof value.question==='string'&&Array.isArray(value.options||[])?'':block; }catch{return block;}
  }).split(streaming&&content.includes(opening)&&!content.slice(content.indexOf(opening)+opening.length).includes('```')?opening:'\u0000')[0].trimEnd();
}

export async function answerDialog(question:ChatQuestion,host:{modal:HTMLDialogElement;escape:Escape;open:(title:string,note:string,body:string)=>void;name:(id:string)=>string;saved:()=>Promise<void>}) {
  const {escape}=host;
  host.open(`${host.name(question.agent_id)} 等待你的回答`,question.prompt,`<form id="chat-question-form">${question.options.length?`<label class="field">选项<select id="chat-question-option"><option value="">自行输入回答</option>${question.options.map(option=>`<option value="${escape(option)}">${escape(option)}</option>`).join('')}</select></label>`:''}<label class="field">回答<textarea id="chat-question-text" rows="3" required placeholder="选择一个选项，或输入你的回答…"></textarea></label><p class="form-error" role="alert"></p><div class="modal-footer"><span>提交后继续此私聊</span><button type="submit" class="primary">提交回答并继续</button></div></form>`);
  const form=host.modal.querySelector<HTMLFormElement>('#chat-question-form')!;
  form.querySelector<HTMLSelectElement>('#chat-question-option')?.addEventListener('change',event=>{form.querySelector<HTMLTextAreaElement>('#chat-question-text')!.value=(event.target as HTMLSelectElement).value;});
  form.addEventListener('submit',async event=>{
    event.preventDefault();const button=form.querySelector<HTMLButtonElement>('button.primary')!;button.disabled=true;
    try {await invoke('answer_chat_question',{conversationId:question.conversation_id,questionId:question.id,content:form.querySelector<HTMLTextAreaElement>('#chat-question-text')!.value});host.modal.close();await host.saved();}
    catch(error){form.querySelector('.form-error')!.textContent=String(error);button.disabled=false;}
  });
}
