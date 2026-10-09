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
    if(command==='remove_conversation_item'){globalThis.__removedItem=args;const workflow=detail.workflows.find(job=>job.id===args.id);if(args.kind==='workflow')detail.workflows=detail.workflows.filter(job=>job.id!==args.id);else detail.messages=detail.messages.filter(message=>message.id!==args.id);return {workflow_id:workflow?.id||null};}
    if(command==='set_project_roles'){detail.project_roles=args.roles;return detail;}
    if(command==='cancel_project_agent'){globalThis.__cancelledAgent=args;const job=detail.workflows.find(job=>job.id===args.workflowId);job.status='interrupted';job.attempts.forEach(attempt=>attempt.status='interrupted');return job;}
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
            expect(page.locator('[data-attempt-id="planner"] .agent-turn-text')).to_contain_text('这是一次普通讨论回复')
            assert page.locator('[data-attempt-id="planner"] .agent-turn-text').inner_text().strip() != '{}'
            assert not page.locator('[data-attempt-id="planner"] .agent-protocol').evaluate('node=>node.open')
            print('PASS planning JSON is readable as a reply with raw protocol data collapsed')
            assert not page.locator('[data-project-history]').count()
            assert not page.locator('.project-agent-room-activity').count()
            assert not page.locator('.bubble').count()
            assert page.locator('.project-agent-chat-messages [data-attempt-id="planner"]').count() == 1
            composer = page.locator('[data-project-agent-chat-form]').bounding_box()
            assert composer and composer['y'] + composer['height'] <= 1000
            print('PASS agent page has one transcript for project and private output, with no execution panel or bubbles')
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
            page.evaluate("""() => {const d=__providerDemo.detail;d.messages=[{id:'old-group-answer',conversation_id:d.conversation.id,sender_id:'codex-win',content:'仅公开回复',thought:'私有思考不显示',tool_events:[{event_id:'tool',title:'私有工具不显示',status:'completed'}],status:'completed',created_at:Date.now()}];}""")
            page.locator('[data-conversation="provider-test"]').click()
            expect(page.locator('[data-message-id="old-group-answer"] .bubble')).to_contain_text('仅公开回复')
            assert not page.locator('#messages .thought,#messages .message-tool-trace').count()
            print('PASS historical group replies retain bubbles while reasoning and tools stay out of the team page')
            transcript=page.evaluate(r"""async () => {
              const {AgentTranscript}=await import('/src/agent_transcript.ts');
              const {agentTimeline}=await import('/src/agent_timeline.ts');
              const result={};const escape=s=>String(s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
              const host=document.createElement('div');host.style.cssText='position:fixed;inset:10px auto auto 10px;width:420px;height:180px;overflow:auto;background:white;z-index:100';document.body.append(host);
              const view=new AgentTranscript(host,{escape,name:s=>s,empty:'<div class="project-agent-chat-empty">empty</div>'});
              const row=(id,body,at=1)=>({id,at,kind:'agent',source:'private',author:'codex-win',body,thought:'',tools:[],status:'completed',error:null,messageId:id});
              const tick=()=>new Promise(r=>setTimeout(r,70));
              let rows=Array.from({length:14},(_,i)=>row('r'+i,'reply '+i+'\n'+'line\n'.repeat(5),i));view.update(rows);await tick();
              result.follows=host.scrollHeight-host.scrollTop-host.clientHeight<3;
              host.dispatchEvent(new WheelEvent('wheel',{deltaY:-100}));host.scrollTop=90;host.dispatchEvent(new Event('scroll'));const anchor=host.scrollTop;rows.at(-1).body+='tail';view.update(rows);await tick();result.manual=host.scrollTop===anchor;
              result.jump=!host.querySelector('.agent-jump-latest').hidden;host.querySelector('.agent-jump-latest').click();await tick();result.returned=host.scrollHeight-host.scrollTop-host.clientHeight<3;
              rows[0].body='same A';view.update(rows);rows[0].body='same B';view.update(rows);result.sameLength=host.querySelector('[data-entry-id="r0"] .agent-turn-text').textContent==='same B';
              const live={...row('live',''),status:'running',thought:Array.from({length:60},(_,i)=>'reason '+i).join('\n'),tools:[{event_id:'t',title:'tool',kind:'command',status:'running',input:'abc',output:'',created_at:1}]};
              view.update([live]);await tick();const thought=host.querySelector('.thought'),body=host.querySelector('.thought-body');result.thoughtFollows=body.scrollHeight-body.scrollTop-body.clientHeight<3;
              body.scrollTop=0;body.dispatchEvent(new Event('scroll'));live.thought+='\nnew reason';view.update([live]);await tick();result.thoughtManual=body.scrollTop===0;
              const tool=host.querySelector('[data-tool-key="all"]');tool.open=true;live.tools[0].status='completed';live.tools[0].input='xyz';view.update([live]);result.toolDisclosure=host.querySelector('[data-tool-key="all"]').open && host.textContent.includes('xyz');
              live.status='completed';live.body='<img src=x onerror="globalThis.__bad=true">';view.update([live]);result.closed=!thought.open;result.safe=!host.querySelector('img')&&!globalThis.__bad;view.latest();live.body+='\n'+'growth\n'.repeat(30);view.update([live]);await tick();result.layoutFollow=host.scrollHeight-host.scrollTop-host.clientHeight<3 && host.querySelector('.agent-jump-latest').hidden;live.body='**bold** and `code`\n\n- one\n- two';view.update([live]);result.markdown=host.querySelector('.agent-turn-text strong')?.textContent==='bold' && host.querySelector('.agent-turn-text code')?.textContent==='code' && host.querySelectorAll('.agent-turn-text li').length===2;
              const d=structuredClone(__providerDemo.detail),chat=structuredClone(__providerDemo.chats['project-chat-codex-win']);chat.messages=[];chat.project_agent_inbox=[];
              const job={...d.workflows[0],created_at:10,attempts:[{...d.workflows[0].attempts[0],id:'later',created_at:300}],agent_messages:[]};chat.messages=[{id:'between',created_at:200,sender_id:'user',content:'private',status:'delivered'}];
              const runtime={active:null};const timeline=agentTimeline('codex-win',chat,[job],runtime);result.order=timeline.map(x=>x.id).join(',')==='request:'+job.id+',message:between,attempt:later';
              view.dispose();host.remove();return result;
            }""")
            assert all(transcript.values()), transcript
            print('PASS unified transcript follows output, preserves manual reading and provides return-to-latest')
            print('PASS reasoning follows live content, preserves manual scroll and collapses on completion')
            print('PASS tool disclosures survive updates and same-length edits render correctly')
            print('PASS project attempts use actual timestamps and output cannot execute markup')
            print('PASS layout-driven scroll keeps auto-follow and Markdown renders without raw HTML')
            page.evaluate("""() => {const d=__providerDemo.detail;d.workflows=[{...d.workflows[0],status:'failed',tasks:[{id:'task',position:0,title:'未完成任务',agent_id:'dsh-win',files:['calc.py'],depends_on:[],status:'queued'}],plan:{summary:'已保存方案',tasks:[]},attempts:[{...d.workflows[0].attempts[0],status:'completed'}]}];}""")
            page.locator('[data-conversation="provider-test"]').click()
            assert not page.locator('[data-edit-project-roles]').count()
            page.locator('[data-continue-project]').click()
            page.wait_for_function("() => __providerDemo.detail.workflows[0].status==='running'")
            assert not page.locator('[data-confirm-project]').count()
            print('PASS paused collaboration resumes its saved plan automatically without stage or task confirmation controls')
            page.evaluate("""() => {const d=__providerDemo.detail;d.workflows=['new-job','old-job'].map((id,index)=>({...d.workflows[0],id,created_at:Date.now()+index,attempts:[{...d.workflows[0].attempts[0],id:id+'-attempt',workflow_id:id,agent_id:'dsh-win',status:'running',thought:'正在执行',output:'',created_at:Date.now()+index}]}));}""")
            page.locator('[data-project-module="dsh-win"]').click()
            page.locator('[data-stop-project-workflow="old-job"]').click()
            page.wait_for_function("() => __cancelledAgent?.workflowId==='old-job' && __cancelledAgent?.agentId==='dsh-win'")
            print('PASS inline interrupt targets the clicked workflow instead of the latest history item')
            page.locator('[data-project-module="team"]').click()
            card=page.locator('[data-workflow-id="old-job"]')
            card.click(button='right')
            page.locator('[data-message-delete]').click()
            page.wait_for_function("() => __removedItem?.kind==='workflow' && __removedItem?.id==='old-job'")
            expect(page.locator('[data-workflow-id="old-job"]')).to_have_count(0)
            print('PASS right-click deletes a round and its cached card does not reappear')
            browser.close()
    finally:
        server.terminate()
        try: server.wait(timeout=5)
        except subprocess.TimeoutExpired: server.kill()


if __name__ == '__main__':
    main()
