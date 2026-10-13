"""Coordinator UI regression fixture. Synthetic IPC only; makes no model requests.

Uses the shared project UI fixture but owns a dedicated Vite port (1423).
Run after coordinator UI selectors and IPC contract have landed.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import subprocess
import time
import urllib.request

from playwright.sync_api import expect, sync_playwright
from verify_model_provider_ui import MOCK

ROOT = Path(__file__).resolve().parents[1]
PORT = 1423
URL = f"http://127.0.0.1:{PORT}"
checks: list[str] = []


def check(name: str, condition: bool) -> None:
    if not condition:
        raise AssertionError(name)
    checks.append(name)
    print("PASS " + name, flush=True)


# Extend the established fake Tauri IPC fixture with the new coordinator DTOs.
# The hidden manager deliberately receives no project_agent_chats entry.
COORDINATOR_FIXTURE = r"""
(() => {
  const demo=globalThis.__providerDemo;
  const original=globalThis.__TAURI_INTERNALS__.invoke;
  const group=demo.group;
  const request='请先检查登录超时问题，再安排成员修复并验证。';
  const taskTitle='检查刷新令牌过期时登录页的实际表现';
  const taskId='dynamic-verification-task';
  const solvedTaskId='completed-solution-task';
  const manager={agent:'hermes-win',model:'openai:gpt-6.1',effort:'high'};
  demo.detail.project_coordinator={agent:'hermes-win',model:null,effort:null};
  demo.detail.project_roles={plan:{agent:'codex-win',model:'gpt-6-luna',effort:'low'},implement:{agent:'dsh-win',model:null,effort:null},review:{agent:'hermes-win',model:null,effort:null}};
  demo.coordinatorState={manager,request,taskTitle,taskId,sets:[],starts:[],continues:[],answers:[],disconnected:[],pending:false};
  demo.detail.coordinator_sessions=[{agent_id:'hermes-win',session_key:'isolated-coordinator',native_session_id:'hidden-session'}];
  globalThis.__TAURI_INTERNALS__.invoke=async(command,args={})=>{
    if(command==='set_project_coordinator'){
      demo.coordinatorState.sets.push(args);
      demo.detail.project_coordinator=args.coordinator;
      return demo.detail;
    }
    if(command==='disconnect_codex'||command==='disconnect_dsh'){
      const agent_id=command==='disconnect_codex'?'codex-win':'dsh-win';
      demo.coordinatorState.disconnected.push(agent_id);
      return {revision:2,connection:'disconnected',executable:null,version:'test',error:null,active:null,models:demo.models[agent_id],default_model:demo.models[agent_id][0]?.id||null,default_effort:demo.models[agent_id][0]?.default_effort||null};
    }
    if(command==='start_project'){
      demo.coordinatorState.starts.push(args);
      const base=await original(command,args);
      const solution={id:solvedTaskId,workflow_id:base.id,position:0,title:'已完成方案：复现刷新令牌过期路径',agent_id:'codex-win',instructions:'确认可复现路径',files:[],depends_on:[],status:'completed',output:'复现步骤已整理',error:null,model:'gpt-6-luna',effort:'low'};
      const task={id:taskId,workflow_id:base.id,position:1,title:taskTitle,agent_id:'codex-win',instructions:'检查登录超时',files:[],depends_on:[0],status:'running',output:'验证成员的连续输出标记',error:null,model:'gpt-6-luna',effort:'low'};
      const job={...base,status:'running',request:args.content,summary:'路由用户原文',plan:{summary:'先验证超时触发路径',tasks:[solution,task]},tasks:[solution,task],coordination:{coordinator:manager,status:'waiting_members',routes:[{task_id:taskId,purpose:'verification',source_id:args.messageId,quote:args.content,context_ids:[]}]},attempts:[
        {id:'hidden-coordinate-attempt',workflow_id:base.id,task_id:null,agent_id:'hermes-win',stage:'plan',activity:'coordinate',status:'running',native_thread_id:'hidden-session',native_turn_id:'coord-turn',model:manager.model,reasoning_effort:manager.effort,thought:'coordination-only-private-thought',tool_events:[],output:'hidden-coordinator-output',error:null},
        {id:'visible-worker-attempt',workflow_id:base.id,task_id:taskId,agent_id:'codex-win',stage:'implement',activity:'task:verification',status:'running',native_thread_id:'worker-session',native_turn_id:'worker-turn',model:'gpt-6-luna',reasoning_effort:'low',thought:'worker-publicly-visible-thought',tool_events:[],output:'验证成员的连续输出标记',error:null}
      ],team_messages:[{id:'public-route-message',project_id:demo.project.id,conversation_id:group.id,workflow_id:base.id,attempt_id:null,source_message_id:args.messageId,sender_agent:'hermes-win',kind:'agent',content:'公开安排：先验证登录超时，再由成员处理。',created_at:Date.now()}]};
      demo.detail.workflows=[job];
      demo.detail.project_team_messages=job.team_messages.slice();
      demo.detail.project_questions=[];
      return job;
    }
    if(command==='pending_chat_questions'){
      if(!demo.coordinatorState.pending)return [];
      return [{kind:'coordination',id:'coord-q',conversation_id:group.id,agent_id:'hermes-win',group_id:group.id}];
    }
    if(command==='answer_project_question'){
      demo.coordinatorState.answers.push(args);
      const q=demo.detail.project_questions.find(item=>item.id===args.questionId);
      if(q){q.status='answered';q.answer=args.content;}
      const job=demo.detail.workflows[0];
      if(job?.coordination)job.coordination.status='routing';
      demo.coordinatorState.pending=false;
      return job;
    }
    if(command==='cancel_project'){
      const job=demo.detail.workflows.find(item=>item.id===args.id);
      if(job){job.status='interrupted';job.coordination.status='paused';for(const a of job.attempts)if(a.status==='running')a.status='interrupted';}
      return job;
    }
    if(command==='continue_project'){
      demo.coordinatorState.continues.push(args);
      const job=demo.detail.workflows.find(item=>item.id===args.workflowId);
      if(job){job.status='running';job.coordination.status='routing';job.error=null;for(const a of job.attempts)if(a.status==='interrupted')a.status='running';}
      return job;
    }
    return original(command,args);
  };
})();
"""


def main() -> None:
    source_hashes = {
        name: hashlib.md5((ROOT / name).read_bytes()).hexdigest()
        for name in ["scripts/verify_coordinator_ui.py", "scripts/verify_model_provider_ui.py", "scripts/verify_project_ui_v17.py"]
        if (ROOT / name).exists()
    }
    server = subprocess.Popen(
        ["node", str(ROOT / "node_modules/vite/bin/vite.js"), "--host", "127.0.0.1", "--port", str(PORT), "--strictPort"],
        cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    success = False
    try:
        for _ in range(100):
            if server.poll() is not None:
                raise RuntimeError(f"Vite exited early with code {server.returncode}; port {PORT} may be occupied")
            try:
                urllib.request.urlopen(URL, timeout=.3).close()
                break
            except OSError:
                time.sleep(.2)
        else:
            raise TimeoutError(f"Vite did not become ready at {URL}")

        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True)
            page = browser.new_page(viewport={"width": 1440, "height": 1000})
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.add_init_script(MOCK)
            page.add_init_script(COORDINATOR_FIXTURE)
            page.goto(URL, wait_until="networkidle")

            project_modules = page.locator("[data-project-module]")
            expect(project_modules).to_have_count(5)
            check("hidden coordinator session creates no project member tab", project_modules.count() == 5 and not page.locator('[data-project-module="isolated-coordinator"]').count())

            page.locator('[data-conversation="provider-test"]').click()
            page.locator('[data-project-configure]').click()
            expect(page.locator("#project-member-settings")).to_be_visible()
            page.locator('[data-project-planner]').select_option("codex-win")
            page.locator('[data-project-coordinator]').select_option("hermes-win")
            page.locator('[data-coordinator-provider]').select_option("openai")
            page.locator('[data-coordinator-model]').select_option("openai:gpt-6.1")
            page.locator('[data-coordinator-effort]').select_option("high")
            page.locator("#project-member-settings button.primary").click()
            page.wait_for_function("() => __providerDemo.coordinatorState.sets.length === 1")
            saved = page.evaluate("() => __providerDemo.coordinatorState.sets[0]")
            check("coordinator agent/model/effort saves through independent coordinator API", saved["coordinator"] == {"agent":"hermes-win","model":"openai:gpt-6.1","effort":"high"})
            check("planner remains a separate default role", page.evaluate("() => __providerDemo.detail.project_roles.plan.agent === 'codex-win'"))
            check("coordinator is not added as a member chat or private conversation", page.evaluate("() => __providerDemo.detail.project_agent_chats.length === 3 && !__providerDemo.detail.conversation.members.includes('isolated-coordinator')"))

            page.locator("#service-link").click()
            page.locator('[data-service-id="codex-win"] [data-service-connect]').click()
            page.wait_for_function("() => __providerDemo.coordinatorState.disconnected.includes('codex-win')")
            page.locator('[data-service-id="dsh-win"] [data-service-connect]').click()
            page.wait_for_function("() => __providerDemo.coordinatorState.disconnected.includes('dsh-win')")
            page.locator("#back-to-chat").click()
            page.locator('[data-project-module="team"]').click()
            check("worker members are disconnected before sending the project goal", page.evaluate("() => __providerDemo.coordinatorState.disconnected.length === 2"))

            request = page.evaluate("() => __providerDemo.coordinatorState.request")
            page.fill("#message-input", request)
            page.locator("#message-input").press("Enter")
            page.wait_for_function("() => __providerDemo.coordinatorState.starts.length === 1")
            routed = page.evaluate("() => __providerDemo.coordinatorState.starts[0].content")
            check("routing starts with the exact user request text", routed == request)
            page.locator('[data-project-module="progress"]').click()
            expect(page.locator(".project-progress-work")).to_contain_text("检查刷新令牌过期时登录页的实际表现")
            check("progress displays dynamic task title without fixed phase naming", page.locator(".project-progress-work").inner_text().find("检查刷新令牌过期时登录页的实际表现") >= 0 and not page.locator(".project-progress-work").get_by_text("验收阶段", exact=True).count())
            check("coordinator status is displayed independently from worker rows", page.locator("[data-project-coordinator-status]").get_attribute("data-status") == "waiting_members" and page.locator(".project-member-status").count() == 3)
            check("active coordinator attempt is excluded from member worker-busy status", not page.locator('.project-member-status[data-progress-agent="hermes-win"]').get_attribute("class").__contains__("working"))
            check("running coordinator workflow does not report full completion", page.locator('[role="progressbar"]').get_attribute("aria-valuenow") != "100")
            page.screenshot(path=str(ROOT / "artifacts/coordinator-progress-wide.png"))

            page.locator('[data-project-module="codex-win"]').click()
            expect(page.locator(".project-agent-chat-messages")).to_contain_text("验证成员的连续输出标记")
            check("worker output remains in its continuous member transcript", page.locator(".project-agent-chat-messages").inner_text().count("验证成员的连续输出标记") >= 1)
            check("hidden coordinator attempt and output do not enter a member transcript", not page.locator('[data-attempt-id="hidden-coordinate-attempt"]').count() and "hidden-coordinator-output" not in page.locator(".project-agent-chat-messages").inner_text())

            page.locator('[data-project-module="team"]').click()
            expect(page.locator('[data-team-message-id="public-route-message"] .bubble')).to_contain_text("公开安排：先验证登录超时")
            check("public team route remains visible as a group chat bubble", page.locator('[data-team-message-id="public-route-message"] .bubble').is_visible())

            page.locator('[data-project-module="progress"]').click()
            question_message = {"id":"coord-q","project_id":"project-test","conversation_id":"provider-test","workflow_id":"unified-workflow","attempt_id":"hidden-coordinate-attempt","source_message_id":"source","sender_agent":"hermes-win","kind":"agent","content":"登录超时后应直接退出，还是保留草稿并提示重新登录？","created_at":int(time.time()*1000)}
            page.evaluate("""m => {const d=__providerDemo.detail;d.workflows[0].coordination.status='awaiting_user';d.project_questions=[{id:'coord-q',workflow_id:'unified-workflow',conversation_id:'provider-test',agent_id:'hermes-win',attempt_id:'hidden-coordinate-attempt',prompt:m.content,options:['直接退出','保留草稿'],status:'pending',answer:null}];d.project_team_messages.push(m);__providerDemo.coordinatorState.pending=true;}""", question_message)
            page.wait_for_function("() => document.querySelector('[data-project-coordinator-status]')?.getAttribute('data-status') === 'awaiting_user'")
            check("progress refreshes manager status from waiting_members to awaiting_user", page.locator('[data-project-coordinator-status]').get_attribute("data-status") == "awaiting_user")
            page.wait_for_function("() => document.querySelector('#question-notice') && !document.querySelector('#question-notice').hidden")
            page.locator("#question-notice").click()
            expect(page.locator('[data-project-module="team"]')).to_have_class(__import__("re").compile("active"))
            page.locator('[data-answer-project-question="coord-q"]').click()
            expect(page.locator("#project-answer-form")).to_be_visible()
            page.locator("#project-answer-text").fill("保留草稿，再提示重新登录。")
            page.locator("#project-answer-form button.primary").click()
            page.wait_for_function("() => __providerDemo.coordinatorState.answers.length === 1")
            check("coordinator question answer returns to the existing workflow", page.evaluate("() => __providerDemo.detail.workflows.length === 1 && __providerDemo.detail.workflows[0].coordination.status === 'routing'"))
            check("answer does not start another plan or forward extra routing instructions", page.evaluate("() => __providerDemo.coordinatorState.starts.length === 1 && __providerDemo.coordinatorState.answers[0].content === '保留草稿，再提示重新登录。'"))

            page.locator('[data-project-module="progress"]').click()
            expect(page.locator('[data-project-coordinator-status]')).to_have_attribute("data-status", "routing")
            check("progress refreshes manager status from awaiting_user to routing after answer", page.locator('[data-project-coordinator-status]').get_attribute("data-status") == "routing")
            page.locator('[data-stop-project="unified-workflow"]').click()
            page.wait_for_function("() => __providerDemo.detail.workflows[0].coordination.status === 'paused'")
            page.locator('[data-continue-project="unified-workflow"]').click()
            page.wait_for_function("() => __providerDemo.coordinatorState.continues.length === 1")
            check("resume reuses the existing task and completed solution without restarting planning", page.evaluate("() => {const w=__providerDemo.detail.workflows[0];return __providerDemo.coordinatorState.starts.length === 1 && w.tasks.some(t => t.id === 'dynamic-verification-task') && w.tasks.find(t => t.id === 'completed-solution-task').status === 'completed' && w.coordination.routes[0].task_id === 'dynamic-verification-task';}"))

            # Match the desktop window's configured minimum width (920px).
            page.set_viewport_size({"width": 920, "height": 760})
            check("minimum-width progress view has no horizontal overflow", page.evaluate("() => document.documentElement.scrollWidth <= innerWidth && document.querySelector('.project-progress-content').clientWidth >= 640"))
            page.screenshot(path=str(ROOT / "artifacts/coordinator-progress-narrow.png"))
            check("no JavaScript errors", not errors)
            after_hashes = {
                name: hashlib.md5((ROOT / name).read_bytes()).hexdigest()
                for name in source_hashes
            }
            check("shared legacy test sources remain byte-identical", all(after_hashes[name] == source_hashes[name] for name in source_hashes if name != "scripts/verify_coordinator_ui.py"))
            success = True
    finally:
        # Exiting sync_playwright above closes its browser before this finally.
        # Always stop the Vite process even if a browser assertion raises.
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait(timeout=5)
        artifacts = ROOT / "artifacts"
        artifacts.mkdir(exist_ok=True)
        (artifacts / "coordinator-ui.json").write_text(
            json.dumps({"success": success, "passed": len(checks), "checks": checks, "port": PORT, "model_requests": 0, "source_md5_before": source_hashes, "source_md5_after": {name: hashlib.md5((ROOT / name).read_bytes()).hexdigest() for name in source_hashes}}, ensure_ascii=False, indent=2),
            encoding="utf-8",
        )


if __name__ == "__main__":
    main()
