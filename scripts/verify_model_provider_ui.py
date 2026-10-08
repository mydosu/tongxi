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
      {id:'gpt-6.1-sol',name:'GPT-6.1 Sol',efforts:['low','high'],default_effort:'low'}
    ],
    'hermes-win':[{id:'openai:gpt-6.1',name:'GPT-6.1',provider_id:'openai',provider_name:'OpenAI',efforts:['low','high'],default_effort:'low'}],
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
  const detail={conversation:group,messages:[],discussions:[],project,workflows:[],sessions:group.members.map(agent_id=>({agent_id,session_key:agent_id,native_session_id:null,model:null,reasoning_effort:null}))};
  globalThis.__providerDemo={detail,models,group,project};
  globalThis.__confirmation=null;
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
    if(command==='get_conversation')return detail;
    if(command==='discussion_status')return null;
    if(command==='project_status')return [];
    if(command.endsWith('_status')){const id=command.startsWith('hermes_')?'hermes-win':command.startsWith('dsh_')?'dsh-win':command.startsWith('albion_')?'albion-wsl':'codex-win';return runtime(id);}
    if(command==='set_session_settings')return detail;
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
            page.locator("#message-input").fill("test")
            page.locator("#send-project").click()
            expect(page.locator("#role-form")).to_be_visible()

            page.locator('[data-role-agent="plan"]').select_option("dsh-win")
            provider_wrap=page.locator('[data-role-provider-wrap="plan"]')
            expect(provider_wrap).to_be_visible()
            provider=page.locator('[data-role-provider="plan"]')
            provider.select_option("route-b")
            model=page.locator('[data-role-model="plan"]')
            expect(model.locator("option")).to_have_count(2)
            assert model.locator("option").nth(1).get_attribute("value") == '["route-b","model-two"]'

            page.locator('[data-role-agent="plan"]').select_option("codex-win")
            expect(provider_wrap).to_be_hidden()
            expect(page.locator('[data-role-model="plan"] option')).to_have_count(3)
            page.locator('[data-role-agent="plan"]').select_option("dsh-win")
            expect(provider_wrap).to_be_visible()
            provider.select_option("route-a")
            model.select_option('["route-a","model-one"]')
            assert model.input_value() == '["route-a","model-one"]'
            page.locator('[data-role-agent="implement"]').select_option("hermes-win")
            assert page.locator('[data-role-agent="implement"]').input_value() == "hermes-win"
            page.locator('[data-role-agent="review"]').select_option("dsh-win")
            assert page.locator('[data-role-agent="review"]').input_value() == "dsh-win"
            print("PASS role agent switching adds/removes provider selector")
            print("PASS planning/execution/review roles can select Hermes and DSH")
            print("PASS provider change limits model options to selected route")

            page.locator("#role-cancel").click()
            page.locator("#live-agent").select_option("dsh-win")
            expect(page.locator("#live-provider")).to_be_visible()
            page.locator("#live-provider").select_option("route-b")
            expect(page.locator("#live-model option")).to_have_count(2)
            assert page.locator("#live-model option").nth(1).get_attribute("value") == '["route-b","model-two"]'
            print("PASS live session provider change limits model options to selected route")

            task={"id":"task-1","workflow_id":"workflow-1","position":0,"title":"实现一个任务","agent_id":"codex-win","instructions":"隔离验证任务","files":["src/example.ts"],"depends_on":[],"execution":{"model":"gpt-6.1-sol","reasoning_effort":"high","rationale":"规划建议"},"status":"queued","output":"","error":None,"model":None,"effort":None,"worktree":None,"branch":None}
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
            print("PASS Codex stage Luna/high takes priority over planner Sol/high suggestion")
            first_agent.select_option("codex-win")
            expect(first_row.locator('[data-task-provider-wrap]')).to_be_hidden()
            task_model=first_row.locator('[data-task-model]')
            expect(task_model.locator("option")).to_have_count(3)
            task_model.select_option("gpt-6-luna")
            first_row.locator('[data-task-effort]').select_option("high")
            first_agent.select_option("hermes-win")
            task_model=first_row.locator('[data-task-model]')
            expect(task_model.locator("option")).to_have_count(2)
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
            assert page.locator('[data-attempt-id="plan-attempt"] summary span').inner_text() == "方案完成"
            assert "任务完成" in page.locator('[data-task-id="task-1"]').inner_text()
            assert "待执行" in page.locator('[data-task-id="task-2"]').inner_text()
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
