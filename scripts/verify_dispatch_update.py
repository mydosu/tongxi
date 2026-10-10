"""Real desktop reproduction: send the executor assignment after planning starts.

Isolated app database/project; only Luna and Daily One. No user repository edits.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from playwright.sync_api import sync_playwright,expect
import smoke_desktop as desktop
import verify_stage3 as prior
import verify_stage6 as groups
from verify_project_v17 import module
from verify_project_workspace import wait_job

ROOT=Path(__file__).resolve().parents[1]
REPORT=ROOT/'artifacts/dispatch-update-live.json'
checks=[];evidence={}
def check(label,condition=True):
    if not condition:raise AssertionError(label)
    checks.append(label);print('PASS '+label,flush=True)

def run():
    proc=None;success=False
    verify_only=os.environ.get('AGENT_HUB_DISPATCH_VERIFY')=='1'
    resume=verify_only or os.environ.get('AGENT_HUB_DISPATCH_RESUME')=='1'
    old=json.loads(REPORT.read_text(encoding='utf-8')) if resume else None
    if resume:
        desktop.DATA=Path(old['data']);checks.extend(old['checks'])
    root=Path(old['evidence']['project_root']) if resume else Path(tempfile.mkdtemp(prefix='tongxi-dispatch-update-'))
    if not resume:
        (root/'calc.py').write_bytes(b'def square(value):\n    return value\n')
        (root/'check.py').write_bytes(b'from calc import square\nassert square(-3)==9\nassert square(0)==0\nassert square(2.5)==6.25\nprint("3 assertions passed")\n')
    desktop.EXE=Path(os.environ.get('AGENT_HUB_EXE',str(ROOT/'src-tauri/target/debug/local-agent-hub.exe')))
    evidence['project_root']=str(root)
    try:
        with sync_playwright() as p:
            proc,endpoint=desktop.launch();browser=p.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser)
            for key in ['codex','dsh','hermes']:
                desktop.ipc(page,'connect_'+key);prior.wait_connection(page,key)
            if resume:
                room=old['evidence']['room'];evidence['room']=room
                page.locator(f'[data-conversation="{room}"]').click();module(page,room,'team')
                prior_job=desktop.ipc(page,'get_conversation',id=room)['workflows'][0]
                if not verify_only:
                    desktop.ipc(page,'continue_project',workflowId=prior_job['id'],autoExecute=True)
                    check('被中断的验收夹具复用原方案与已完成DSH改动')
            else:
                project=desktop.ipc(page,'register_project',name='补充分工真实验收',root=str(root))
                room=groups.group(page,'补充分工真实验收',['codex-win','dsh-win','hermes-win'])
                evidence['room']=room
                desktop.ipc(page,'bind_project',conversationId=room,projectId=project['id'])
                page.locator(f'[data-conversation="{room}"]').click();module(page,room,'team')
                detail=desktop.ipc(page,'get_conversation',id=room)
                for chat in detail['project_agent_chats']:
                    agent=chat['agent_id'];key={'codex-win':'codex','dsh-win':'dsh','hermes-win':'hermes'}[agent]
                    models=desktop.ipc(page,key+'_status')['models']
                    model=next(m for m in models if m['id']=='gpt-5.6-luna') if key=='codex' else next(m for m in models if (m.get('provider_id')=='command-code-daily' or 'daily one' in (m.get('provider_name') or '').lower()) and 'v4.1' in m['id'])
                    desktop.ipc(page,'set_session_settings',id=chat['conversation_id'],agentId=agent,model=model['id'],reasoningEffort='low')
                page.fill('#message-input','我想在使用这个计算工具的同时继续开发改进它，但具体首项改进还未选定。由Codex出方案，Hermes执行并验收。先了解现状，让我回答具体要实现什么，再开始实际实现；不要用只读摸底或复核任务冒充开发完成。无需项目记忆检索或写入，这只是临时测试夹具。')
                page.locator('#message-input').press('Enter')
                deadline=time.monotonic()+60
                while time.monotonic()<deadline:
                    detail=desktop.ipc(page,'get_conversation',id=room)
                    if detail['workflows'] and any(a['stage']=='plan' and a['status'] in ['starting','running'] for a in detail['workflows'][0]['attempts']):break
                    page.wait_for_timeout(100)
                else:raise TimeoutError('未进入真实规划')
                workflow=detail['workflows'][0]
                check('先启动规划，初始执行角色为Hermes',json.loads(workflow['roles'])['implement']['agent']=='hermes-win')
                page.fill('#message-input','由codex出方案，dsh执行，Hermes做好验收和文件整理')
                page.locator('#message-input').press('Enter')
                page.wait_for_function('async id=>{const d=await window.__TAURI_INTERNALS__.invoke("get_conversation",{id});return JSON.parse(d.workflows[0].roles).implement.agent==="dsh-win";}',arg=room)
                check('规划中的补充气泡立即更新实际执行角色为DSH')
                deadline=time.monotonic()+600;question=None
                while time.monotonic()<deadline:
                    detail=desktop.ipc(page,'get_conversation',id=room)
                    question=next((q for q in detail['project_questions'] if q['status']=='pending'),None)
                    if question:break
                    if detail['workflows'][0]['status'] in ['completed','failed','interrupted']:raise AssertionError('澄清前协作已终止：'+str(detail['workflows'][0].get('error')))
                    page.wait_for_timeout(250)
                if not question:raise TimeoutError('规划者未澄清具体目标')
                check('缺少具体目标时规划者真实提问，未跳到Hermes验收',question['agent_id']=='codex-win' and not any(a['stage'] in ['implement','review'] for a in detail['workflows'][0]['attempts']))
                expect(page.locator(f'[data-team-message-id="{question["id"]}"]')).to_be_visible(timeout=15000)
                page.screenshot(path=str(ROOT/'artifacts/dispatch-update-question.png'))
                page.fill('#message-input','首项改进：修复calc.py的square(value)，返回平方，支持负数、零和小数，仅修改calc.py。请分一个DSH实现任务，Hermes实际运行python -B check.py独立验收。无需进一步提问，不调用项目记忆工具。')
                page.locator('#message-input').press('Enter')
            done=wait_job(page,room,lambda job:job['status']=='completed',timeout=1200)
            evidence['workflow']=done
            implementations=[a for a in done['attempts'] if a['stage']=='implement']
            check('DSH承担真实实现，Codex和Hermes未代替实现',bool(implementations) and all(a['agent_id']=='dsh-win' for a in implementations) and all(t['status']=='completed' for t in done['tasks']))
            check('实际任务和显示的执行者一致',all(t['agent_id']=='dsh-win' for t in done['tasks']))
            changed_files=set()
            for task in done['tasks']:
                if task.get('branch'):
                    changed_files.update(subprocess.run(['git','show','--format=','--name-only',task['branch']],cwd=root,capture_output=True,text=True,timeout=20,check=True).stdout.splitlines())
            check('DSH实际调用工具且Git提交确认改动calc.py',any(a.get('tool_events') for a in implementations) and 'calc.py' in changed_files)
            check('实现之后Hermes独立验收',any(a['stage']=='review' and a['agent_id']=='hermes-win' and a['status']=='completed' for a in done['attempts']))
            check('没有重新创建规划轮次，复用原会话和已生成内容',len([a for a in done['attempts'] if a['stage']=='plan' and not a.get('activity')])==1)
            check('模型请求未使用Sol',all('sol' not in (a.get('model') or '').lower() for a in done['attempts']))
            check('负数零小数三个功能断言通过',subprocess.run(['python','-B','check.py'],cwd=root,capture_output=True,timeout=20).returncode==0)
            module(page,room,'progress');expect(page.locator('[role="progressbar"]')).to_have_attribute('aria-valuenow','100')
            page.screenshot(path=str(ROOT/'artifacts/dispatch-update-complete.png'))
            check('真实窗口没有JavaScript错误',not desktop.errors)
            success=True
    finally:
        if proc and proc.poll() is None:proc.terminate();proc.wait(timeout=10)
        REPORT.write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks,'data':str(desktop.DATA),'sha256':hashlib.sha256(desktop.EXE.read_bytes()).hexdigest(),'evidence':evidence},ensure_ascii=False,indent=2),encoding='utf-8')

if __name__=='__main__':run()
