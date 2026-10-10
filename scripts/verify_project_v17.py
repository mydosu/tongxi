"""Real isolated v0.17 project UI, shared native sessions, and question/resume acceptance.

Calls only Luna and Command Code Daily One. Never touches the production database.
"""
import hashlib
import json
import os
import re
import sqlite3
from pathlib import Path
import subprocess
import tempfile
import time
import uuid
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_stage3 as prior
import verify_stage6 as groups
from verify_project_workspace import wait_job, wait_private

ROOT = Path(__file__).resolve().parents[1]
checks = []
evidence = {}
errors = []

def check(name, condition=True):
    if not condition:
        raise AssertionError(name)
    checks.append(name)
    print('PASS ' + name, flush=True)

def module(page, room, name):
    target = page.locator(f'[data-project-module="{name}"][data-group-id="{room}"]')
    if not target.is_visible():
        page.locator(f'[data-conversation="{room}"]').click()
    target.click()

def run():
    proc = None
    success = False
    verify_only = os.environ.get('AGENT_HUB_TEST_VERIFY') == '1'
    reuse = verify_only or os.environ.get('AGENT_HUB_TEST_RESUME') == '1'
    old = json.loads((ROOT/'artifacts/project-v17-live.json').read_text(encoding='utf-8')) if reuse else None
    if reuse: desktop.DATA = Path(old['data'])
    if verify_only: checks.extend(old['checks'])
    root = Path(old['evidence']['project_root']) if reuse else Path(tempfile.mkdtemp(prefix='tongxi-v17-project-'))
    if not reuse:
        (root/'calc.py').write_text('def square(value):\n    return value\n', encoding='utf-8')
        (root/'check.py').write_text('from calc import square\nassert square(-3)==9\nassert square(0)==0\nassert square(2.5)==6.25\nprint("3 assertions passed")\n', encoding='utf-8')
    desktop.EXE = Path(os.environ.get('AGENT_HUB_EXE', str(ROOT/'src-tauri/target/debug/local-agent-hub.exe')))
    evidence['project_root'] = str(root)
    try:
        with sync_playwright() as p:
            proc, endpoint = desktop.launch()
            browser = p.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on('pageerror', lambda error: errors.append(str(error)))
            for agent in ('codex', 'dsh', 'hermes'):
                desktop.ipc(page, 'connect_' + agent)
                prior.wait_connection(page, agent)
            if reuse:
                with sqlite3.connect(desktop.DATA/'hub.db') as db:
                    room = db.execute('SELECT conversation_id FROM workflows ORDER BY created_at DESC LIMIT 1').fetchone()[0]
                page.locator(f'[data-conversation="{room}"]').click()
            else:
                project = desktop.ipc(page, 'register_project', name='共享项目会话验收', root=str(root))
                room = groups.group(page, '开发协作验收', ['codex-win','dsh-win','hermes-win'])
                desktop.ipc(page, 'bind_project', conversationId=room, projectId=project['id'])
                page.locator(f'[data-conversation="{room}"]').click()
                check('绑定普通目录自动建立 Git 仓库', (root/'.git').exists())
                parent = page.locator(f'[data-conversation="{room}"]')
                parent.click()
                expect(page.locator(f'[data-project-module="progress"][data-group-id="{room}"]')).to_be_hidden()
                parent.click()
                expect(page.locator(f'[data-project-module="progress"][data-group-id="{room}"]')).to_be_visible()
                check('开发协作主按钮收起和展开，没有独立展开按钮', not page.locator('[data-project-expand]').count())
            states = {agent: desktop.ipc(page, key+'_status') for agent,key in [('codex-win','codex'),('dsh-win','dsh'),('hermes-win','hermes')]}
            models = {
                'codex-win': next(m for m in states['codex-win']['models'] if m['id']=='gpt-5.6-luna'),
                'dsh-win': next(m for m in states['dsh-win']['models'] if m.get('provider_id')=='command-code-daily' and 'v4.1' in m['id']),
                'hermes-win': next(m for m in states['hermes-win']['models'] if 'daily one' in (m.get('provider_name') or '').lower() and 'v4.1' in m['id'])
            }
            if not reuse:
                page.locator('[data-project-configure]').click()
                expect(page.locator('#project-member-settings')).to_be_visible()
                for agent,model in models.items():
                    row = page.locator(f'[data-settings-agent="{agent}"]')
                    if row.locator('[data-member-provider]').count():
                        row.locator('[data-member-provider]').select_option(model['provider_id'])
                    row.locator('[data-member-model]').select_option(model['id'])
                    row.locator('[data-member-effort]').select_option('low')
                page.screenshot(path=str(ROOT/'artifacts/v17-project-member-config.png'))
                page.locator('#project-member-settings button.primary').click()
                expect(page.locator('#modal')).not_to_be_visible()
            detail = desktop.ipc(page,'get_conversation',id=room)
            chats = {row['agent_id']:row['conversation_id'] for row in detail['project_agent_chats']}
            check('集中配置为三个成员保存模型和强度', all(desktop.ipc(page,'get_conversation',id=chat)['sessions'][0]['model']==models[agent]['id'] for agent,chat in chats.items()))
            markers = {}
            sessions = {}
            if reuse:
                for agent,chat in chats.items():
                    saved=desktop.ipc(page,'get_conversation',id=chat)
                    markers[agent]=re.search(r'PRIVATE_[a-f0-9]+',next(row['content'] for row in saved['messages'] if row['sender_id']=='user')).group()
                    sessions[agent]=saved['sessions'][0]['native_session_id']
            else:
                for agent,chat in chats.items():
                    module(page,room,agent)
                    expect(page.locator('[data-project-agent-chat-input]')).to_be_enabled()
                    check(agent+' 专属页没有模型选择或项目流程框', not page.locator('[data-project-chat-model],.workflow-card,.project-agent-room-activity').count())
                    markers[agent] = 'PRIVATE_' + uuid.uuid4().hex[:12]
                    before = desktop.ipc(page, {'codex-win':'codex','dsh-win':'dsh','hermes-win':'hermes'}[agent]+'_status').get('active')
                    page.fill('[data-project-agent-chat-input]', f'记住本项目私聊标记 {markers[agent]}。之后在项目工作回复中确认这个标记。现在只回复标记，不用工具，不广播。')
                    page.locator('[data-project-agent-chat-input]').press('Enter')
                    turn,_ = wait_private(page,agent,chat,after_id=before['id'] if before else None)
                    check(agent+' 真实项目私聊完成', markers[agent] in turn['text'])
                    sessions[agent] = desktop.ipc(page,'get_conversation',id=chat)['sessions'][0]['native_session_id']
            module(page,room,'team')
            prompt = '修复calc.py的square(value)返回平方，支持负数零小数，只改calc.py。Codex规划一个任务、DSH实现、Hermes验收运行python -B check.py。每个成员在自己的最终输出中确认之前私聊记住的标记，这次允许分享这个测试标记。DSH在实际修改之前必须调用hub_ask_user向用户询问函数名用square还是squared，结束本轮操作等待回答；若同一会话中已有这个问题的真实回答则直接遵从回答继续，不能再次提问。规划勿代替DSH提问。模型使用项目配置，不自行换模型。'
            if not verify_only:
                if reuse:
                    desktop.ipc(page,'continue_project',workflowId=detail['workflows'][0]['id'],autoExecute=True)
                    check('resume retains the original plan',len(desktop.ipc(page,'get_conversation',id=room)['workflows'][0]['attempts'])>=len(detail['workflows'][0]['attempts']))
                else:
                    page.fill('#message-input',prompt)
                    page.locator('#message-input').press('Enter')
                deadline = time.monotonic()+600
                question = None
                while time.monotonic()<deadline:
                    detail = desktop.ipc(page,'get_conversation',id=room)
                    question = next((q for q in detail.get('project_questions',[]) if q['status']=='pending'),None)
                    if question: break
                    if detail['workflows'] and detail['workflows'][0]['status'] in ['failed','completed','interrupted']:
                        raise AssertionError('没有进入提问等待：'+str(detail['workflows'][0].get('error')))
                    page.wait_for_timeout(300)
                if not question: raise TimeoutError('没有收到真实提问')
                check('DSH 真实工具提问在群聊可见',question['agent_id']=='dsh-win')
                expect(page.locator(f'[data-team-message-id="{question["id"]}"]')).to_be_visible(timeout=15000)
                check('提问时源码尚未修改', (root/'calc.py').read_text(encoding='utf-8')=='def square(value):\n    return value\n')
                page.wait_for_timeout(1800)
                check('等待用户回答期间协作保持活动',desktop.ipc(page,'get_conversation',id=room)['workflows'][0]['status']=='running')
                module(page,room,'progress')
                expect(page.locator('.project-member-status')).to_have_count(3)
                expect(page.locator('.project-progress-content')).to_contain_text('等待你的回答')
                page.screenshot(path=str(ROOT/'artifacts/v17-progress-waiting.png'))
                check('独立进度页显示成员状态和任务进度')
                module(page,room,'team')
                check('团队页没有项目流程卡片，问题仍为头像气泡',not page.locator('.workflow-card,[data-workflow-id]').count() and page.locator(f'[data-team-message-id="{question["id"]}"] .bubble').count()==1)
                page.fill('#message-input','使用square，不需要改名。')
                page.locator('#message-input').press('Enter')
            done = wait_job(page,room,lambda job:job['status']=='completed',timeout=900)
            evidence['workflow'] = done
            check('回答后从同一协作继续并完成三个阶段', {a['stage'] for a in done['attempts'] if a['status']=='completed'} >= {'plan','implement','review'})
            for agent,chat in chats.items():
                attempts=[a for a in done['attempts'] if a['agent_id']==agent and a['status']=='completed']
                current=desktop.ipc(page,'get_conversation',id=chat)['sessions'][0]
                check(agent+' 私聊和项目执行共用原生会话 ID',bool(sessions[agent]) and all(a['native_thread_id']==sessions[agent]==current['native_session_id'] for a in attempts))
                check(agent+' 执行上下文记得私聊标记',any(markers[agent] in a['output'] for a in attempts))
                check(agent+' 项目配置的模型未被规划覆盖',all(a['model']==models[agent]['id'] and a['reasoning_effort']=='low' for a in attempts))
            check('真实测试未调用Sol',all('sol' not in (a.get('model') or '').lower() for a in done['attempts']))
            result=subprocess.run(['python','-B','check.py'],cwd=root,capture_output=True,timeout=20)
            check('实际项目负数零小数三个断言通过',result.returncode==0)
            expect(page.locator('[data-team-message-id="result:'+done['id']+'"]')).to_be_visible(timeout=15000)
            check('Hermes最终结果发布到团队头像气泡')
            page.screenshot(path=str(ROOT/'artifacts/v17-team-complete.png'))
            module(page,room,'progress')
            expect(page.locator('[role="progressbar"]')).to_have_attribute('aria-valuenow','100')
            page.screenshot(path=str(ROOT/'artifacts/v17-progress-complete.png'))
            module(page,room,'dsh-win')
            expect(page.locator('[data-attempt-id]').last).to_be_attached()
            page.screenshot(path=str(ROOT/'artifacts/v17-dsh-transcript.png'))
            check('成员页推理工具回复在同一连续会话区且无气泡',not page.locator('.bubble,.workflow-card').count())
            for width in [980,1440]:
                page.set_viewport_size({'width':width,'height':950})
                check(str(width)+'宽度无水平溢出',page.evaluate('document.documentElement.scrollWidth<=innerWidth'))
            check('窗口无JavaScript错误',not errors and not desktop.errors)
            success=True
    finally:
        if proc and proc.poll() is None:
            proc.terminate();proc.wait(timeout=10)
        (ROOT/'artifacts/project-v17-live.json').write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks,'errors':errors,'data':str(desktop.DATA),'sha256':hashlib.sha256(desktop.EXE.read_bytes()).hexdigest(),'evidence':evidence},ensure_ascii=False,indent=2),encoding='utf-8')

if __name__=='__main__':
    run()
