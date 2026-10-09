"""隔离验证多提供商选择和项目角色切换；浏览器 IPC 使用本地固定演示数据。"""
from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import time
import urllib.request

from playwright.sync_api import expect, sync_playwright

ROOT = Path(__file__).resolve().parents[1]
PORT = "1420"

MOCK = r"""
(() => {
  const now=Date.now();
  const models={
    'codex-win':[
      {id:'gpt-6-luna',name:'GPT-6 Luna',efforts:['low','high'],default_effort:'low'},
      {id:'gpt-5.6-luna',name:'GPT-5.6 Luna',efforts:['low','high'],default_effort:'low'}
    ],
    'hermes-win':[
      {id:'custom:command-code-daily:deepseek/deepseek-v4.1-flash',name:'DeepSeek V4.1 Flash',provider_id:'command-code-daily',provider_name:'Command Code Daily One',efforts:['none','low','high'],default_effort:'low'},
      {id:'openai:gpt-6.1',name:'GPT-6.1',provider_id:'openai',provider_name:'OpenAI',efforts:['low','high'],default_effort:'low'}
    ],
    'dsh-win':[
      {id:'["route-a","model-one"]',name:'Model One',provider_id:'route-a',efforts:['off','low'],default_effort:'off'},
      {id:'["route-b","model-two"]',name:'Model Two',provider_id:'route-b',efforts:['off','low'],default_effort:'off'}
    ],
    'albion-wsl':[{id:'qwen3',name:'qwen3',efforts:[],default_effort:null}]
  };
  const agents=[
    {id:'hermes-win',name:'Hermes',subtitle:'管家',role:'协调与检查',location:'Windows',accent:'amber',status:'not_connected'},
    {id:'codex-win',name:'Codex',subtitle:'方案',role:'规划与诊断',location:'Windows',accent:'green',status:'not_connected'},
    {id:'dsh-win',name:'DSH',subtitle:'实现',role:'编码与修复',location:'Windows',accent:'blue',status:'not_connected'},
    {id:'albion-wsl',name:'阿尔比恩',subtitle:'陪伴',role:'交流支持',location:'WSL',accent:'rose',status:'not_connected'}
  ];
  const group={id:'provider-test',title:'模型选择回归',kind:'group',archived:false,created_at:now,updated_at:now,members:['hermes-win','codex-win','dsh-win'],message_count:0,preview:''};
  const project={id:'project-test',name:'隔离项目',root:'D:\\Demo',checks:[],summary_enabled:false,created_at:now};
  const detail={conversation:group,messages:[],discussions:[],project,workflows:[],project_agent_chats:group.members.map(agent_id=>({agent_id,conversation_id:'project-chat-'+agent_id})),project_team_messages:[],sessions:group.members.map(agent_id=>({agent_id,session_key:agent_id,native_session_id:null,model:null,reasoning_effort:null}))};
  const chatDetails={};
  globalThis.__providerDemo={detail,models,group,project,chats:chatDetails};
  globalThis.__confirmation=null;
  globalThis.__collaborationRequest=null;
  globalThis.__collaborationCalls=0;
  globalThis.__continueCalls=0;
  globalThis.__roleUpdate=null;
  const runtime=id=>({revision:1,connection:'connected',executable:null,version:'test',error:null,active:null,models:models[id],default_model:models[id][0]?.id||null,default_effort:models[id][0]?.default_effort||null});
  let callback=0;
  globalThis.isTauri=true;
  globalThis.__TAURI_INTERNALS__={metadata:{currentWindow:{label:'main'}},transformCallback(){return ++callback;},invoke:async(command,args={})=>{
    if(command==='plugin:event|listen'||command==='plugin:window|hide'||command==='plugin:window|close')return 1;
    if(command==='app_info')return {version:'test',milestone:7,database_path:'',hud_shortcut:false};
    if(command==='list_agents')return agents;
    if(command==='list_conversations')return [group];
    if(command==='get_conversation'){
      if(String(args.id||'').startsWith('project-chat-')){
        const agent_id=String(args.id).slice('project-chat-'.length);
        return chatDetails[args.id] ||= {conversation:{id:args.id,title:'隔离项目 · '+agent_id,kind:'direct',archived:false,created_at:now,updated_at:now,members:[agent_id],message_count:0,preview:''},messages:[],discussions:[],project:null,project_roles:null,workflows:[],project_agent_chats:[],project_team_messages:[],sessions:[{agent_id,session_key:agent_id,native_session_id:null,model:null,reasoning_effort:null}]};
      }
      return detail;
    }
    if(command==='discussion_status')return null;
    if(command==='project_status')return [];
    if(command.endsWith('_status')){const id=command.startsWith('hermes_')?'hermes-win':command.startsWith('dsh_')?'dsh-win':command.startsWith('albion_')?'albion-wsl':'codex-win';return runtime(id);}
    if(command==='set_session_settings'){const current=chatDetails[args.id]||detail;const session=current.sessions.find(s=>s.agent_id===args.agentId);session.model=args.model;session.reasoning_effort=args.reasoningEffort;return current;}
    if(command==='set_project_roles'){detail.project_roles=args.roles;return detail;}
    if(command==='start_project'){
      globalThis.__collaborationRequest=args;
      globalThis.__collaborationCalls++;
      const get=agent=>{const session=chatDetails['project-chat-'+agent].sessions[0];return {agent,model:session.model,effort:session.reasoning_effort};};const roles={plan:get('codex-win'),implement:get('dsh-win'),review:get('hermes-win')};
      const workflow={id:'unified-workflow',project_id:project.id,conversation_id:group.id,user_message_id:args.messageId,request:args.content,status:'completed',plan:{summary:'这是一次普通讨论回复，不需要改文件。',tasks:[]},roles:JSON.stringify(roles),summary:'这是一次普通讨论回复，不需要改文件。',error:null,created_at:now,updated_at:now,tasks:[],attempts:[{id:'planner',workflow_id:'unified-workflow',task_id:null,agent_id:roles.plan.agent,stage:'plan',status:'completed',native_thread_id:'session-1',native_turn_id:'turn-1',model:roles.plan.model,reasoning_effort:roles.plan.effort,thought:'规划阶段思考增量',output:'{}',error:null}],changes:[],agent_messages:[]};
      detail.workflows=[workflow];
      return workflow;
    }
    if(command==='confirm_project'){
      globalThis.__confirmation=args.tasks;
      const workflow=detail.workflows[0];
      for(const choice of args.tasks){const task=workflow.tasks.find(t=>t.position===choice.position);Object.assign(task,choice);}
      workflow.status='running';workflow.updated_at=Date.now();return workflow;
    }
    if(command==='continue_project'){
      globalThis.__continueCalls++;
      const workflow=detail.workflows.find(item=>item.id===args.workflowId);
      workflow.status=args.autoExecute?'running':'planning';workflow.plan={summary:'复用原方案',tasks:workflow.tasks.map(task=>({...task}))};for(const task of workflow.tasks)if(task.status!=='completed')task.status='queued';workflow.error=null;workflow.updated_at=Date.now();return workflow;
    }
    if(command==='update_paused_project_roles'){
      globalThis.__roleUpdate=args.roles;
      const workflow=detail.workflows.find(item=>item.id===args.workflowId);
      workflow.roles=JSON.stringify(args.roles);workflow.updated_at=Date.now();return workflow;
    }
    return null;
  }};
})();
"""


def main() -> None:
    server = subprocess.Popen(f"npm run dev -- --host 127.0.0.1 --port {PORT} --strictPort", cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, shell=True, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    try:
        for _ in range(100):
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{PORT}", timeout=.3).read(20)
                break
            except Exception:
                time.sleep(.2)
        with sync_playwright() as p:
            browser=p.chromium.launch(headless=True)
            page=browser.new_page(viewport={"width":1440,"height":1000})
            page.add_init_script(MOCK)
            page.goto(f"http://127.0.0.1:{PORT}",wait_until="networkidle")
            expect(page.locator('[data-project-module]')).to_have_count(4)
            assert not page.locator('#project-roles').count()
            expect(page.locator('#live-settings')).to_be_hidden()
            print('PASS team navigation exposes project agent modules without stage controls or duplicate model controls')
            page.locator('[data-project-expand]').click()
            expect(page.locator('[data-project-module="team"]')).to_be_hidden()
            page.locator('[data-project-expand]').click()
            expect(page.locator('[data-project-module="team"]')).to_be_visible()
            print('PASS sidebar project modules expand and collapse')
            page.locator('[data-project-module="dsh-win"]').click()
            page.locator('[data-project-chat-provider]').select_option('route-b')
            page.wait_for_function("() => __providerDemo.chats['project-chat-dsh-win'].sessions[0].model === '[\"route-b\",\"model-two\"]'")
            assert page.locator('[data-project-chat-model]').input_value() == '["route-b","model-two"]'
            expect(page.locator('[data-project-chat-model] option')).to_have_count(2)
            print('PASS changing only provider saves an explicit model from that provider instead of the old default')
            page.locator('[data-project-chat-effort]').select_option('low')
            page.wait_for_function("() => __providerDemo.chats['project-chat-dsh-win'].sessions[0].reasoning_effort === 'low'")
            expect(page.locator('.project-agent-chat-empty')).to_be_visible()
            artifacts=ROOT/'artifacts';artifacts.mkdir(exist_ok=True)
            page.screenshot(path=str(artifacts/'project-agent-empty-wide.png'))
            page.set_viewport_size({"width":980,"height":760})
            assert page.evaluate("() => document.documentElement.scrollWidth <= innerWidth")
            page.screenshot(path=str(artifacts/'project-agent-empty-narrow.png'))
            page.set_viewport_size({"width":1440,"height":1000})
            print('PASS agent page controls and empty state fit wide and narrow windows')
            page.locator('[data-project-module="codex-win"]').click()
            page.locator('[data-project-chat-model]').select_option('gpt-6-luna')
            page.locator('[data-project-chat-effort]').select_option('low')
            page.wait_for_function("() => __providerDemo.chats['project-chat-codex-win'].sessions[0].reasoning_effort === 'low'")
            page.locator('[data-project-module="hermes-win"]').click()
            page.locator('[data-project-chat-provider]').select_option('command-code-daily')
            page.locator('[data-project-chat-effort]').select_option('high')
            page.wait_for_function("() => __providerDemo.chats['project-chat-hermes-win'].sessions[0].reasoning_effort === 'high'")
            print('PASS each agent project session saves its own model and effort')
            page.locator('[data-project-module="team"]').click()
            page.fill('#message-input','Codex规划，DSH执行，Hermes验收')
            page.locator('#message-input').press('Enter')
            page.wait_for_function('() => __collaborationCalls === 1')
            assert not page.locator('[data-confirm-project]').count()
            assert not page.locator('#role-form').count()
            roles=page.evaluate('() => JSON.parse(__providerDemo.detail.workflows[0].roles)')
            assert roles['plan']['model']=='gpt-6-luna' and roles['implement']['model']=='["route-b","model-two"]' and roles['review']['effort']=='high'
            print('PASS Enter starts one automatic collaboration with per-agent settings and no role/plan modal')
            assert not page.locator('[data-thought-attempt-id]').count()
            page.locator('[data-project-module="codex-win"]').click()
            expect(page.locator('[data-thought-attempt-id="planner"]')).to_be_attached()
            print('PASS reasoning and full project execution output stay in the assigned agent module')
            assert not page.locator('[data-project-history]').evaluate('node => node.open')
            composer = page.locator('[data-project-agent-chat-form]').bounding_box()
            assert composer and composer['y'] + composer['height'] <= 1000
            page.locator('[data-project-history] > summary').click()
            expect(page.locator('[data-attempt-id="planner"]')).to_be_visible()
            print('PASS completed history collapses to preserve conversation space and can be expanded')
            page.locator('[data-project-module="team"]').click()
            page.evaluate("""() => { const chat=__providerDemo.chats['project-chat-codex-win'];chat.messages=Array.from({length:24},(_,i)=>({id:'old-'+i,conversation_id:chat.conversation.id,sender_id:'codex-win',content:'历史消息 '+i+'。'.repeat(100),thought:'',status:'completed',created_at:Date.now()+i})); }""")
            page.locator('[data-project-module="codex-win"]').click()
            page.wait_for_function("() => {const n=document.querySelector('.project-agent-chat-messages');return n && n.scrollHeight-n.scrollTop-n.clientHeight<3;}")
            print('PASS opening an existing member transcript starts at its latest message')
            page.locator('[data-project-module="team"]').click()
            page.evaluate("""() => {const d=__providerDemo.detail;const row={id:'team-visible',project_id:d.project.id,conversation_id:d.conversation.id,workflow_id:d.workflows[0].id,attempt_id:'planner',source_message_id:null,sender_agent:'hermes-win',kind:'agent',content:'真实消息布局回归',created_at:Date.now()};d.project_team_messages=[row];d.workflows[0].team_messages=[row];d.workflows.push({...d.workflows[0],id:'history',team_messages:[row]});}""")
            page.locator('[data-conversation="provider-test"]').click()
            row=page.locator('[data-team-message-id="team-visible"]')
            expect(row).to_be_visible()
            assert row.count()==1 and row.locator('.avatar').count()==1 and row.locator('.bubble').count()==1
            assert row.evaluate("node => !node.closest('.workflow-card')")
            print('PASS public messages remain visible once as avatar bubbles outside collapsed workflow cards')
            scroll=page.evaluate(r"""async () => {const {bindProjectThoughtFollow,unbindProjectThoughtFollow}=await import('/src/project_thought_scroll.ts');const host=document.createElement('div');host.innerHTML='<details class="attempt-thought" data-active="true" open><div class="attempt-thought-output" data-follow="true" style="height:60px;overflow:auto"><div class="attempt-thought-content" style="white-space:pre-wrap"></div></div></details>';document.body.append(host);const body=host.querySelector('.attempt-thought-output'),content=host.querySelector('.attempt-thought-content');content.textContent=Array.from({length:30},(_,i)=>'思考行'+i).join('\n');bindProjectThoughtFollow(host);await new Promise(r=>setTimeout(r,60));const follows=body.scrollHeight-body.scrollTop-body.clientHeight<3;body.scrollTop=0;body.dispatchEvent(new Event('scroll'));await new Promise(requestAnimationFrame);bindProjectThoughtFollow(host);content.textContent+='\n新增思考';await new Promise(r=>setTimeout(r,60));const preserves=body.scrollTop<3;unbindProjectThoughtFollow(host);host.remove();return {follows,preserves};}""")
            assert scroll=={'follows':True,'preserves':True}
            print('PASS streamed reasoning follows live output and preserves manual reading after rebind')
            page.evaluate("""() => {const d=__providerDemo.detail;d.workflows=[{...d.workflows[0],status:'failed',tasks:[{id:'task',position:0,title:'未完成任务',agent_id:'dsh-win',files:['calc.py'],depends_on:[],status:'queued'}],plan:{summary:'已保存方案',tasks:[]},attempts:[{...d.workflows[0].attempts[0],status:'completed'}]}];}""")
            page.locator('[data-conversation="provider-test"]').click()
            assert not page.locator('[data-edit-project-roles]').count()
            page.locator('[data-continue-project]').click()
            page.wait_for_function("() => __providerDemo.detail.workflows[0].status==='running'")
            assert not page.locator('[data-confirm-project]').count()
            print('PASS paused collaboration resumes its saved plan automatically without stage or task confirmation controls')
            browser.close()
    finally:
        server.terminate()
        try: server.wait(timeout=5)
        except subprocess.TimeoutExpired: server.kill()


if __name__ == '__main__':
    main()
