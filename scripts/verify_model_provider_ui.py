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
    {id:'hermes-win',name:'Hermes',subtitle:'管家',role:'协调与检查',location:'Windows',accent:'hermes',status:'not_connected'},
    {id:'codex-win',name:'Codex',subtitle:'方案',role:'规划与诊断',location:'Windows',accent:'codex',status:'not_connected'},
    {id:'dsh-win',name:'DSH',subtitle:'实现',role:'编码与修复',location:'Windows',accent:'dsh',status:'not_connected'},
    {id:'albion-wsl',name:'阿尔比恩',subtitle:'陪伴',role:'交流支持',location:'WSL',accent:'albion',status:'not_connected'}
  ];
  const group={id:'provider-test',title:'模型选择回归',kind:'group',archived:false,created_at:now,updated_at:now,members:['hermes-win','codex-win','dsh-win'],message_count:0,preview:''};
  const project={id:'project-test',name:'隔离项目',root:'D:\\Demo',checks:[],summary_enabled:false,created_at:now};
  const detail={conversation:group,messages:[],discussions:[],project,workflows:[],project_agent_chats:group.members.map(agent_id=>({agent_id,conversation_id:'project-chat-'+agent_id})),project_team_messages:[],sessions:group.members.map(agent_id=>({agent_id,session_key:agent_id,native_session_id:null,model:null,reasoning_effort:null}))};
  globalThis.__providerDemo={detail,models,group,project};
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
        return {conversation:{id:args.id,title:'隔离项目 · '+agent_id,kind:'direct',archived:false,created_at:now,updated_at:now,members:[agent_id],message_count:0,preview:''},messages:[],discussions:[],project:null,project_roles:null,workflows:[],project_agent_chats:[],project_team_messages:[],sessions:[{agent_id,session_key:agent_id,native_session_id:null,model:null,reasoning_effort:null}]};
      }
      return detail;
    }
    if(command==='discussion_status')return null;
    if(command==='project_status')return [];
    if(command.endsWith('_status')){const id=command.startsWith('hermes_')?'hermes-win':command.startsWith('dsh_')?'dsh-win':command.startsWith('albion_')?'albion-wsl':'codex-win';return runtime(id);}
    if(command==='set_session_settings')return detail;
    if(command==='set_project_roles'){detail.project_roles=args.roles;return detail;}
    if(command==='start_project'){
      globalThis.__collaborationRequest=args;
      globalThis.__collaborationCalls++;
      const roles=detail.project_roles;
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
      workflow.status='planning';workflow.plan={summary:'复用原方案',tasks:workflow.tasks.map(task=>({...task}))};for(const task of workflow.tasks)if(task.status!=='completed')task.status='queued';workflow.error=null;workflow.updated_at=Date.now();return workflow;
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
    env = os.environ.copy()
    server = subprocess.Popen(f"npm run dev -- --host 127.0.0.1 --port {PORT} --strictPort", cwd=ROOT, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, shell=True, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    try:
        for _ in range(100):
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{PORT}", timeout=0.3).read(20)
                break
            except Exception:
                time.sleep(0.2)
        else:
            raise RuntimeError("Vite 页面未启动")
        with sync_playwright() as p:
            browser=p.chromium.launch(headless=True)
            page=browser.new_page(viewport={"width":1440,"height":1000})
            page.add_init_script(MOCK)
            page.goto(f"http://127.0.0.1:{PORT}",wait_until="networkidle")
            expect(page.locator("#project-controls")).to_be_visible()
            expect(page.locator('.project-module-row')).to_have_count(4)
            expect(page.locator('[data-project-module="team"]')).to_be_visible()
            page.locator('[data-project-expand="provider-test"]').click()
            expect(page.locator('[data-project-module="team"]')).to_be_hidden()
            page.locator('[data-project-expand="provider-test"]').click()
            expect(page.locator('[data-project-module="team"]')).to_be_visible()
            print("PASS project group expands in the sidebar into team and per-agent modules")
            page.locator('[data-project-module="dsh-win"]').click()
            expect(page.locator('.project-agent-room-header h1')).to_contain_text('DSH · 项目专属会话')
            expect(page.locator('.project-agent-chat-messages')).to_be_visible()
            assert page.locator('#message-input').count() == 0, "agent module must replace the group composer with its own project conversation"
            expect(page.locator('[data-conversation="provider-test"]')).to_have_class(__import__('re').compile(r'\bactive\b'))
            print("PASS selecting an agent sidebar module opens its own project conversation outside the group timeline")
            page.locator('[data-project-module="team"]').click()
            expect(page.locator('#project-controls')).to_be_visible()
            assert page.locator('.project-tabs').count() == 0, "project navigation must not be embedded in the conversation"
            page.locator("#project-roles").click()
            expect(page.locator("#role-form")).to_be_visible()
            page.locator('[data-role-agent="plan"]').select_option("hermes-win")
            provider=page.locator('[data-role-provider="plan"]')
            expect(provider).to_be_visible()
            provider.select_option("command-code-daily")
            model=page.locator('[data-role-model="plan"]')
            model.select_option("custom:command-code-daily:deepseek/deepseek-v4.1-flash")
            page.locator('[data-role-effort="plan"]').select_option("high")
            page.locator("#role-form .primary").click()
            page.wait_for_function("() => __providerDemo.detail.project_roles?.plan?.model === 'custom:command-code-daily:deepseek/deepseek-v4.1-flash'")
            assert page.evaluate("() => __providerDemo.detail.project_roles.plan.effort") == "high"
            print("PASS planning/execution/review agents and their models persist as project-group settings")
            print("PASS a stage can route Hermes through Command Code Daily One")
            page.locator("#message-input").fill("test")
            assert page.locator("#send-project").count() == 0, "bound project rooms use one shared composer"
            page.locator("#send-discussion").click()
            page.wait_for_function("() => __collaborationCalls === 1")
            expect(page.locator(".workflow-card").first).to_be_visible()
            assert page.evaluate("() => __collaborationRequest.content") == "test"
            assert page.evaluate("() => __providerDemo.detail.workflows[0].roles") is not None
            assert page.evaluate("() => __providerDemo.detail.workflows[0].attempts[0].model") == "custom:command-code-daily:deepseek/deepseek-v4.1-flash"
            page.locator('[data-project-module="hermes-win"]').click()
            thought=page.locator('[data-thought-attempt-id=planner]')
            thought.wait_for(state="attached")
            assert thought.count()==1 and '规划阶段思考增量' in thought.text_content()
            print("PASS the assigned agent's sidebar module displays persisted streaming thought and output")
            page.locator('[data-project-module="team"]').click()
            thought_ui=page.evaluate("""async () => {
              const {workflowCard,projectAgentActivity}=await import('/src/project_ui.ts');
              const {bindProjectThoughtFollow,unbindProjectThoughtFollow}=await import('/src/project_thought_scroll.ts');
              const base=globalThis.__providerDemo.detail.workflows[0];
              const render=job=>{const host=document.createElement('div');host.innerHTML=workflowCard(job,x=>String(x),x=>x==='hermes-win'?'Hermes':x==='dsh-win'?'DSH':String(x),()=>[],[],[],[]);return host.firstElementChild;};
              const renderAgent=job=>{const host=document.createElement('div');host.innerHTML=projectAgentActivity(job,'hermes-win',x=>String(x),x=>x==='hermes-win'?'Hermes':String(x),(_,value)=>value||'model');return host.firstElementChild;};
              const workspaceHost=document.createElement('div');
              const workspaceJob=structuredClone(base);workspaceJob.status='running';workspaceJob.attempts[0].status='running';
              workspaceHost.innerHTML=workflowCard(workspaceJob,x=>String(x),x=>x==='hermes-win'?'Hermes':x==='dsh-win'?'DSH':String(x),()=>[],[],[],[{id:'team-1',project_id:'p',conversation_id:'g',workflow_id:base.id,attempt_id:'planner',source_message_id:null,sender_agent:'hermes-win',kind:'agent',content:'已开始分工',created_at:Date.now()}],id=>'<i>'+id+'</i>');
              const teamPanel=workspaceHost.querySelector('[data-project-panel=team]');
              const workspace={collapsible:!!workspaceHost.querySelector('details[data-project-accordion]'),conversationTabs:workspaceHost.querySelectorAll('[data-project-tab]').length,privatePanels:workspaceHost.querySelectorAll('[data-project-chat-id]').length,teamHasThought:!!teamPanel.querySelector('[data-thought-attempt-id]'),agentHasThought:!!renderAgent(workspaceJob).querySelector('[data-thought-attempt-id]'),teamMessage:teamPanel.textContent.includes('已开始分工')};
              workspaceHost.remove();
              const settled=render(base);
              const settledClosed=!renderAgent(base).querySelector('[data-thought-attempt-id=planner]').open;
              const agentStatus=renderAgent(base).querySelector('[data-attempt-id=planner] summary')?.textContent.includes('方案完成')||false;
              const approvedJob=structuredClone(base);approvedJob.summary='已对照需求和源码完成验收。';approvedJob.attempts.push({...approvedJob.attempts[0],id:'hermes-review',agent_id:'hermes-win',stage:'review',output:'{"approved":true}'});
              const reviewerLabel=render(approvedJob).querySelector('.workflow-result')?.textContent.includes('Hermes 验收结论')||false;
              const activeJob=structuredClone(base);activeJob.attempts[0].status='running';
              const active=renderAgent(activeJob);
              const activeOpen=active.querySelector('[data-thought-attempt-id=planner]').open;
              const card=document.createElement('article');
              card.innerHTML='<details class="attempt-thought" data-active="true" open><div class="attempt-thought-output" style="height:60px;overflow:auto"><div class="attempt-thought-content" style="white-space:pre-wrap"></div></div></details>';
              document.body.append(card);
              const body=card.querySelector('.attempt-thought-output'), content=card.querySelector('.attempt-thought-content');
              content.textContent=Array.from({length:35},(_,i)=>'思考行 '+i).join('\\n');
              bindProjectThoughtFollow(card);
              await new Promise(resolve=>setTimeout(resolve,60));
              const firstFollows=body.scrollHeight-body.scrollTop-body.clientHeight<=2;
              const sameBody=body===card.querySelector('.attempt-thought-output');
              body.scrollTop=0;body.dispatchEvent(new Event('scroll'));await new Promise(requestAnimationFrame);
              content.textContent+='\\n'+Array.from({length:10},(_,i)=>'新增行 '+i).join('\\n');
              await new Promise(resolve=>setTimeout(resolve,60));
              const manualScrollKept=body.scrollTop<=2;
              unbindProjectThoughtFollow(card);card.remove();
              return {workspace,settledClosed,agentStatus,reviewerLabel,activeOpen,firstFollows,sameBody,manualScrollKept};
            }""")
            assert thought_ui == {"workspace":{"collapsible":True,"conversationTabs":0,"privatePanels":0,"teamHasThought":False,"agentHasThought":True,"teamMessage":True},"settledClosed":True,"agentStatus":True,"reviewerLabel":True,"activeOpen":True,"firstFollows":True,"sameBody":True,"manualScrollKept":True}, thought_ui
            print("PASS project reasoning follows live growth, keeps manual scroll, and collapses completed thoughts")
            print("PASS completed project result visibly identifies Hermes as the reviewer")
            print("PASS project workspace separates the shared team feed from per-agent thought modules")
            assert not page.locator("#modal").evaluate("el => el.open"), "each message starts the unified collaboration without another role prompt"
            print("PASS project-bound group uses one composer for discussion and implementation")
            print("PASS discussion-only intent returns in the project timeline without fake tasks")
            scroll = page.evaluate("""async () => {
              const {captureThoughtScroll, restoreThoughtScroll} = await import('/src/thought_scroll.ts');
              const message=document.createElement('article');
              message.innerHTML='<details class="thought" open><summary>思考中</summary><div class="thought-body"></div></details>';
              document.body.append(message);
              const body=message.querySelector('.thought-body');
              body.style.height='44px'; body.style.maxHeight='44px'; body.style.overflow='auto'; body.style.whiteSpace='pre-wrap';
              body.textContent=Array.from({length:30},(_,i)=>'推理内容 '+i).join('\\n');
              body.scrollTop=body.scrollHeight;
              const follow=captureThoughtScroll(message);
              body.textContent += '\\n'+Array.from({length:20},(_,i)=>'新增推理 '+i).join('\\n');
              restoreThoughtScroll(message,follow);
              await new Promise(requestAnimationFrame); await new Promise(requestAnimationFrame);
              const follows = body.scrollHeight-body.scrollTop-body.clientHeight <= 2;
              body.scrollTop=0;
              const reading=captureThoughtScroll(message);
              body.textContent += '\\n'+Array.from({length:20},(_,i)=>'后续推理 '+i).join('\\n');
              restoreThoughtScroll(message,reading);
              await new Promise(requestAnimationFrame); await new Promise(requestAnimationFrame);
              const keepsReading = body.scrollTop <= 2;
              message.remove();
              return {follows,keepsReading};
            }""")
            assert scroll == {"follows": True, "keepsReading": True}, scroll
            print("PASS reasoning block follows new text at bottom and preserves manual scroll position")

            page.locator("#live-agent").select_option("dsh-win")
            expect(page.locator("#live-provider")).to_be_visible()
            page.locator("#live-provider").select_option("route-b")
            expect(page.locator("#live-model option")).to_have_count(2)
            assert page.locator("#live-model option").nth(1).get_attribute("value") == '["route-b","model-two"]'
            print("PASS live session provider change limits model options to selected route")

            task={"id":"task-1","workflow_id":"workflow-1","position":0,"title":"实现一个任务","agent_id":"codex-win","instructions":"隔离验证任务","files":["src/example.ts"],"depends_on":[],"execution":{"model":"gpt-5.6-luna","reasoning_effort":"high","rationale":"规划建议"},"status":"queued","output":"","error":None,"model":None,"effort":None,"worktree":None,"branch":None}
            second={**task,"id":"task-2","position":1,"title":"第二项","agent_id":"hermes-win","files":["src/second.ts"],"execution":None}
            roles={"plan":{"agent":"codex-win","model":None,"effort":None},"implement":{"agent":"codex-win","model":"gpt-6-luna","effort":"high"},"review":{"agent":"hermes-win","model":None,"effort":None}}
            workflow={"id":"workflow-1","project_id":"project-test","conversation_id":"provider-test","user_message_id":"request-1","request":"隔离验证","status":"planning","plan":{"summary":"演示分工","tasks":[task,second]},"roles":json.dumps(roles),"summary":"","error":None,"created_at":1,"updated_at":1,"tasks":[task,second],"attempts":[],"changes":[]}
            page.evaluate("workflow => { __providerDemo.detail.workflows=[workflow]; }", workflow)
            page.locator('[data-conversation="provider-test"]').click()
            first_row=page.locator('[data-task-position="0"]')
            first_agent=first_row.locator('[data-task-agent-select]')
            expect(first_agent).to_be_visible()
            task_model=first_row.locator('[data-task-model]')
            assert task_model.input_value() == "gpt-6-luna", "stage role model must take priority over the planner suggestion"
            assert first_row.locator('[data-task-effort]').input_value() == "high"
            print("PASS Codex stage Luna/high takes priority over alternate Luna/high suggestion")
            first_agent.select_option("codex-win")
            expect(first_row.locator('[data-task-provider-wrap]')).to_be_hidden()
            task_model=first_row.locator('[data-task-model]')
            expect(task_model.locator("option")).to_have_count(3)
            task_model.select_option("gpt-6-luna")
            first_row.locator('[data-task-effort]').select_option("high")
            first_agent.select_option("hermes-win")
            task_model=first_row.locator('[data-task-model]')
            expect(task_model.locator("option")).to_have_count(2)
            first_row.locator('[data-task-provider]').select_option("openai")
            task_model.select_option("openai:gpt-6.1")
            first_agent.select_option("dsh-win")
            expect(first_row.locator('[data-task-provider-wrap]')).to_be_visible()
            first_row.locator('[data-task-provider]').select_option("route-b")
            task_model=first_row.locator('[data-task-model]')
            expect(task_model.locator("option")).to_have_count(2)
            task_model.select_option('["route-b","model-two"]')
            first_agent.select_option("codex-win")
            task_model=first_row.locator('[data-task-model]')
            assert task_model.input_value() == "gpt-6-luna"
            assert first_row.locator('[data-task-effort]').input_value() == "high"
            task_model.select_option("gpt-6-luna")
            first_row.locator('[data-task-effort]').select_option("high")
            page.locator('[data-task-position="1"] [data-task-provider]').select_option("openai")
            page.locator('[data-task-position="1"] [data-task-model]').select_option("openai:gpt-6.1")
            page.locator('[data-confirm-project]').click()
            page.wait_for_function("() => Array.isArray(__confirmation) && __confirmation.length === 2")
            confirmed=page.evaluate("() => __confirmation")
            assert confirmed[0]["agent_id"] == "codex-win" and confirmed[0]["model"] == "gpt-6-luna" and confirmed[0]["effort"] == "high"
            assert confirmed[1]["agent_id"] == "hermes-win" and confirmed[1]["model"] == "openai:gpt-6.1"
            print("PASS Codex project task submits selected GPT-6 Luna/high unchanged")
            print("PASS per-task Hermes selection remains independent")

            recovery={**workflow,"status":"failed","plan":workflow["plan"],"error":"执行失败","attempts":[{"id":"plan-attempt","workflow_id":"workflow-1","agent_id":"codex-win","stage":"plan","status":"completed","native_thread_id":None,"native_turn_id":"turn-1","model":"gpt-6.1","reasoning_effort":"high","output":"saved plan output","checks":[],"error":None}],"updated_at":int(time.time()*1000)}
            recovery["tasks"][0].update(status="completed",worktree="previous-worktree",branch="hub/previous-task")
            recovery["tasks"][1]["status"]="skipped"
            page.evaluate("job => { __providerDemo.detail.workflows=[job]; }", recovery)
            page.locator('[data-conversation="provider-test"]').click()
            edit=page.locator('[data-edit-project-roles]')
            expect(edit).to_be_visible()
            edit.click()
            expect(page.locator('#role-form')).to_be_visible()
            assert page.locator('[data-role-model="implement"]').input_value() == "gpt-6-luna"
            assert page.locator('[data-role-effort="implement"]').input_value() == "high"
            page.locator('[data-role-agent="review"]').select_option("dsh-win")
            page.locator('[data-role-provider="review"]').select_option("route-b")
            page.locator('[data-role-model="review"]').select_option('["route-b","model-two"]')
            page.locator('[data-role-effort="review"]').select_option("low")
            page.locator('#role-form .primary').click()
            page.wait_for_function("() => __roleUpdate && __roleUpdate.review.agent === 'dsh-win'")
            changed_roles=page.evaluate("() => __roleUpdate")
            assert changed_roles["review"]["model"] == '["route-b","model-two"]' and changed_roles["review"]["effort"] == "low"
            assert changed_roles["implement"]["model"] == "gpt-6-luna"
            print("PASS paused workflow edits all phase agent/model/effort choices")
            resume=page.locator('[data-continue-project]')
            expect(resume).to_be_visible()
            assert "不重新调用规划模型" in resume.get_attribute("title")
            resume.click()
            expect(page.locator('[data-confirm-project]')).to_be_visible()
            expect(page.locator('[data-task-position="0"] [data-task-agent-select]')).to_be_disabled()
            expect(page.locator('[data-task-position="1"] [data-task-agent-select]')).to_be_enabled()
            assert page.evaluate("() => __continueCalls") == 1
            completed_task_text=page.locator('[data-task-id="task-1"]').text_content() or ''
            queued_task_text=page.locator('[data-task-id="task-2"]').text_content() or ''
            assert "任务完成" in completed_task_text, repr(completed_task_text)
            assert "待执行" in queued_task_text, repr(queued_task_text)
            print("PASS failed project resumes from saved plan and returns to task confirmation")
            print("PASS plan/task status labels do not claim the whole collaboration is complete")
            browser.close()
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()


if __name__ == "__main__":
    main()
