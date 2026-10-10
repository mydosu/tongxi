"""Interrupt a real project after completed work, discuss correction, and finish revised goal.

Uses the synthetic project and native member sessions created by verify_project_v17.
"""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import time
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_stage3 as prior
from verify_project_workspace import wait_job
from verify_project_v17 import module

ROOT=Path(__file__).resolve().parents[1]
checks=[];evidence={}
def check(name,condition=True):
    if not condition: raise AssertionError(name)
    checks.append(name);print('PASS '+name,flush=True)

def run():
    report=json.loads((ROOT/'artifacts/project-v17-live.json').read_text(encoding='utf-8'))
    desktop.DATA=Path(report['data'])
    desktop.EXE=Path(os.environ.get('AGENT_HUB_EXE',str(ROOT/'src-tauri/target/debug/local-agent-hub.exe')))
    root=Path(report['evidence']['project_root'])
    with sqlite3.connect(desktop.DATA/'hub.db') as db:
        room=db.execute('SELECT conversation_id FROM workflows ORDER BY created_at DESC LIMIT 1').fetchone()[0]
    proc=None;success=False
    try:
        with sync_playwright() as p:
            proc,endpoint=desktop.launch();browser=p.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser)
            for key in ['codex','dsh','hermes']:
                desktop.ipc(page,'connect_'+key);prior.wait_connection(page,key)
            page.locator(f'[data-conversation="{room}"]').click();module(page,room,'team')
            revised_only=os.environ.get('AGENT_HUB_TEST_CORRECTION_REVISED')=='1'
            if revised_only:
                detail=desktop.ipc(page,'get_conversation',id=room);job=next(job for job in detail['workflows'] if job['status'] in ['failed','interrupted'] and any(task['status']=='completed' for task in job['tasks']))
            else:
                if os.environ.get('AGENT_HUB_TEST_CORRECTION_RESUME')=='1':
                    detail=desktop.ipc(page,'get_conversation',id=room)
                    desktop.ipc(page,'continue_project',workflowId=detail['workflows'][0]['id'],autoExecute=True)
                else:
                    page.fill('#message-input','隔离项目测试：Codex规划恰好两个有依赖任务，DSH实现，Hermes验收。任务1创建identity.py，identity(value)直接返回value，运行python -B -c断言identity(7)==7。任务2依赖任务1，创建cube.py，但开始修改前必须hub_ask_user问用户是否创建cube，然后结束本轮工具操作等待真正回答，不要猜。每任务只修改一个对应文件。保留calc.py和check.py。模型不变。')
                    page.locator('#message-input').press('Enter')
                deadline=time.monotonic()+900;detail=None
                while time.monotonic()<deadline:
                    detail=desktop.ipc(page,'get_conversation',id=room);job=detail['workflows'][0]
                    if any(q['status']=='pending' and q['workflow_id']==job['id'] for q in detail['project_questions']): break
                    if job['status'] in ['failed','completed','interrupted']:raise AssertionError('提前结束：'+str(job.get('error')))
                    page.wait_for_timeout(300)
                else:raise TimeoutError('等待第二任务提问超时')
            evidence['old_workflow']=job['id']
            completed_file=Path(job['tasks'][0]['worktree'])/'identity.py'
            if not completed_file.exists():completed_file=root/'identity.py'
            check('提问前第一任务已完成并提交',len(job['tasks'])==2 and job['tasks'][0]['status']=='completed' and completed_file.exists())
            original=hashlib.sha256(completed_file.read_bytes()).hexdigest()
            if not revised_only:
                module(page,room,'progress');page.locator('[data-stop-project]').click()
                stopped=wait_job(page,room,lambda job:job['status']=='interrupted',allow_pause=True)
            check('暂停结束待答问题并保留已完成代码',not any(q['status']=='pending' for q in desktop.ipc(page,'get_conversation',id=room)['project_questions']) and completed_file.exists())
            module(page,room,'team')
            page.fill('#message-input','纠正方向：不要再创建cube.py。保留已完成的identity.py和之前的平方功能。改成创建double.py，double(value)返回两倍值。先由成员讨论调整建议，然后Codex规划一个DSH任务，DSH实现，Hermes实际运行python -B -c验证double(-3)==-6和double(0)==0，并确认identity(7)==7和check.py。无须再提问。需要回改旧内容时先公开说明。模型维持项目配置。')
            page.locator('#message-input').press('Enter')
            done=wait_job(page,room,lambda job:job['status']=='completed',timeout=900)
            discussions=[a for a in done['attempts'] if a.get('activity')=='discussion']
            check('纠正后DSH和Hermes先讨论再规划',{a['agent_id'] for a in discussions}=={'dsh-win','hermes-win'} and all(a['status']=='completed' for a in discussions))
            check('讨论建议已发给团队与规划成员',bool(done.get('agent_messages')) and any(m.get('attempt_id') in [a['id'] for a in discussions] for m in done.get('team_messages',[])))
            check('已完成改动字节保持不变',hashlib.sha256((root/'identity.py').read_bytes()).hexdigest()==original)
            result=subprocess.run(['python','-B','-c','from identity import identity; from double import double; assert identity(7)==7; assert double(-3)==-6; assert double(0)==0'],cwd=root,capture_output=True,timeout=20)
            check('新方向完成三个功能断言且未创建已取消文件',result.returncode==0 and not (root/'cube.py').exists())
            check('最终独立Hermes验收正常',any(a['stage']=='review' and a['agent_id']=='hermes-win' and a['status']=='completed' for a in done['attempts']))
            check('讨论任务和验收均未使用Sol',all('sol' not in (a.get('model') or '').lower() for a in done['attempts']))
            evidence['revised_workflow']=done
            module(page,room,'team');page.screenshot(path=str(ROOT/'artifacts/v17-corrected-team.png'))
            module(page,room,'progress');page.screenshot(path=str(ROOT/'artifacts/v17-corrected-progress.png'))
            success=True
    finally:
        if proc and proc.poll() is None:proc.terminate();proc.wait(timeout=10)
        (ROOT/'artifacts/project-v17-correction.json').write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks,'evidence':evidence,'data':str(desktop.DATA)},ensure_ascii=False,indent=2),encoding='utf-8')

if __name__=='__main__':run()
