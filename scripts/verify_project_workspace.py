"""Real project collaboration, private tools, broadcast, and selective interruption.

Uses only synthetic fixture data and explicit Luna / Command Code Daily One models.
The original failed isolated workflow is resumed instead of paying for another plan.
"""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time
import uuid

from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_stage3 as prior
import verify_stage6 as groups

ROOT = Path(__file__).resolve().parents[1]
REPORT = ROOT / 'artifacts/project-workspace-live-verification.json'
checks = []
evidence = {}
errors = []


def check(label, condition=True):
    if not condition:
        raise AssertionError(label)
    checks.append(label)
    print('PASS ' + label, flush=True)


def select_team(page, room):
    page.locator(f'[data-conversation="{room}"]').click()
    page.locator(f'[data-project-module="team"][data-group-id="{room}"]').click()
    expect(page.locator('#message-input')).to_be_visible()


def select_agent(page, agent):
    room = page.evaluate("localStorage.getItem('hub.selected')")
    page.locator(f'[data-project-module="{agent}"][data-group-id="{room}"]').click()
    expect(page.locator('.project-agent-chat-messages')).to_be_visible()


def configure_agents(page, room, roles):
    select_team(page, room)
    detail = desktop.ipc(page, 'get_conversation', id=room)
    for role in roles.values():
        agent = role['agent']
        select_agent(page, agent)
        state = desktop.ipc(page, {'codex-win':'codex','dsh-win':'dsh','hermes-win':'hermes'}[agent] + '_status')
        model = next(model for model in state['models'] if model['id'] == role['model'])
        if page.locator('[data-project-chat-provider]').count():
            page.select_option('[data-project-chat-provider]', model.get('provider_id') or '__default__')
        page.select_option('[data-project-chat-model]', role['model'])
        page.select_option('[data-project-chat-effort]', role['effort'])
        chat_id = next(chat['conversation_id'] for chat in detail['project_agent_chats'] if chat['agent_id'] == agent)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            session = desktop.ipc(page, 'get_conversation', id=chat_id)['sessions'][0]
            if session['model'] == role['model'] and session['reasoning_effort'] == role['effort']: break
            page.wait_for_timeout(100)
        check(agent + ' 的项目模型和强度由专属页保存', session['model'] == role['model'] and session['reasoning_effort'] == role['effort'])
    select_team(page, room)
    check('团队和暂停阶段配置按钮均已移除', not page.locator('#project-roles').count() and not page.locator('[data-edit-project-roles]').count())


def latest(page, room):
    return desktop.ipc(page, 'get_conversation', id=room)['workflows'][0]


def wait_job(page, room, predicate, timeout=900, allow_pause=False, capture=False, workflow_id=None):
    deadline = time.monotonic() + timeout
    last_log = 0
    captured = False
    while time.monotonic() < deadline:
        job = next(job for job in desktop.ipc(page, 'get_conversation', id=room)['workflows'] if job['id'] == workflow_id) if workflow_id else latest(page, room)
        if predicate(job):
            return job
        if job['status'] == 'failed' or (job['status'] == 'interrupted' and not allow_pause):
            evidence['failed_workflow'] = {'status': job['status'], 'error': job.get('error'), 'attempts': [{'agent': a['agent_id'], 'stage': a['stage'], 'status': a['status'], 'model': a['model']} for a in job['attempts']]}
            raise AssertionError('协作提前结束：' + str(job.get('error')))
        if time.monotonic() - last_log > 20:
            print('LIVE ' + json.dumps({'status': job['status'], 'tasks': [t['status'] for t in job['tasks']], 'attempts': [(a['agent_id'], a['stage'], a['status'], len(a.get('thought', ''))) for a in job['attempts']]}, ensure_ascii=False), flush=True)
            last_log = time.monotonic()
        if capture and not captured and any(a['agent_id'] == 'dsh-win' and len(a.get('thought', '')) > 100 for a in job['attempts'] if a['status'] == 'running'):
            select_agent(page, 'dsh-win')
            page.screenshot(path=str(ROOT / 'artifacts/live-project-dsh-stream.png'))
            select_team(page, room)
            captured = True
        page.wait_for_timeout(250)
    raise TimeoutError('等待协作状态超时')


def wait_private(page, agent, chat_id, timeout=480, after_id=None):
    deadline = time.monotonic() + timeout
    key = {'codex-win': 'codex', 'hermes-win': 'hermes', 'dsh-win': 'dsh'}[agent]
    observed_live = False
    while time.monotonic() < deadline:
        state = desktop.ipc(page, key + '_status')
        active = state.get('active')
        if active and active['conversation_id'] == chat_id and active['id'] != after_id:
            if active['status'] in ('starting', 'running', 'cancelling'):
                observed_live = True
            else:
                if active['status'] != 'completed':
                    raise AssertionError('原生私聊失败：' + str(active.get('error')))
                return active, observed_live
        page.wait_for_timeout(250)
    raise TimeoutError('项目私聊超时')


def resume_failed(page, room):
    before = latest(page, room)
    if REPORT.exists():
        original_id = json.loads(REPORT.read_text(encoding='utf-8')).get('evidence', {}).get('collaboration', {}).get('workflow_id')
        before = next((job for job in desktop.ipc(page, 'get_conversation', id=room)['workflows'] if job['id'] == original_id), before)
    plans = len([a for a in before['attempts'] if a['stage'] == 'plan'])
    select_team(page, room)
    if before['status'] != 'completed':
        page.locator(f'[data-continue-project="{before["id"]}"]').click()
        done = wait_job(page, room, lambda job: job['status'] == 'completed', capture=True, workflow_id=before['id'])
    else:
        done = before
    check('失败协作复用原方案，规划调用次数没有增加', len([a for a in done['attempts'] if a['stage'] == 'plan']) == plans)
    check('DSH 原生实现成功完成', any(a['stage'] == 'implement' and a['agent_id'] == 'dsh-win' and a['status'] == 'completed' for a in done['attempts']))
    check('全部实现后 Hermes 自动验收并返回结论', any(a['stage'] == 'review' and a['agent_id'] == 'hermes-win' and a['status'] == 'completed' for a in done['attempts']) and bool(done['summary']))
    check('真实协作未使用 Sol', all('sol' not in (a.get('model') or '').lower() for a in done['attempts']))
    project = desktop.ipc(page, 'get_conversation', id=room)['project']
    result = subprocess.run(['python', 'check.py'], cwd=project['root'], capture_output=True, timeout=20)
    check('实际项目通过平方、负数和零的 3 个功能断言', result.returncode == 0)
    result_card = page.locator(f'[data-workflow-id="{done["id"]}"] .workflow-result')
    expect(result_card).to_contain_text('Hermes 验收结论')
    check('验收结论无须展开卡片即可看见', result_card.is_visible())
    select_agent(page, 'dsh-win')
    expect(page.locator('[data-agent-direct-message-id]').first).to_be_attached()
    check('DSH 到 Hermes 的真实交接消息在成员模块可查', bool(done.get('agent_messages')))
    page.screenshot(path=str(ROOT / 'artifacts/live-project-dsh-complete.png'))
    select_agent(page, 'hermes-win')
    expect(page.locator('[data-attempt-id]').last).to_be_attached()
    page.screenshot(path=str(ROOT / 'artifacts/live-project-hermes-review.png'))
    select_team(page, room)
    page.screenshot(path=str(ROOT / 'artifacts/live-project-team-complete.png'))
    evidence['collaboration'] = {'workflow_id': done['id'], 'plan_attempts': plans, 'roles': json.loads(done['roles']), 'attempts': [{'agent': a['agent_id'], 'stage': a['stage'], 'status': a['status'], 'model': a['model'], 'thought_chars': len(a.get('thought', '')), 'tools': len(a.get('tool_events', []))} for a in done['attempts']]}


def private_tools(page, room):
    detail = desktop.ipc(page, 'get_conversation', id=room)
    chat = next(c['conversation_id'] for c in detail['project_agent_chats'] if c['agent_id'] == 'codex-win')
    select_team(page, room)
    select_agent(page, 'codex-win')
    initial_model = os.environ.get('AGENT_HUB_TEST_LUNA', 'gpt-6-luna')
    if 'luna' not in initial_model.lower(): raise ValueError('此验收只允许 Luna 模型')
    page.select_option('[data-project-chat-model]', initial_model)
    page.select_option('[data-project-chat-effort]', 'low')
    page.wait_for_timeout(250)
    private = 'PRIVATE_' + uuid.uuid4().hex[:10]
    public = 'PUBLIC_' + uuid.uuid4().hex[:10]
    direct = 'DIRECT_' + uuid.uuid4().hex[:10]
    models = desktop.ipc(page, 'codex_status')['models']
    alternate = os.environ.get('AGENT_HUB_TEST_NEXT_LUNA') or next((m['id'] for m in models if 'luna' in m['id'].lower() and m['id'] != initial_model), initial_model)
    if 'luna' not in alternate.lower(): raise ValueError('此验收只允许 Luna 模型')
    prompt = f'这是隔离项目验收。私有标记 {private} 只在本会话保留。请实际读取当前项目的 calc.py 并通过命令运行 check.py，确认3个断言通过；然后调用 hub_message 给 hermes-win 发送含 {direct} 的验收交接，再调用 hub_broadcast 给团队发送一句含 {public} 的验证成功报告，不要在广播中包含私有或直发标记。最终回复说明调用了哪些工具。'
    previous_run = desktop.ipc(page, 'codex_status').get('active')
    previous_id = previous_run['id'] if previous_run else None
    page.fill('[data-project-agent-chat-input]', prompt)
    page.locator('[data-project-agent-chat-input]').press('Enter')
    for _ in range(60):
        current = desktop.ipc(page, 'codex_status').get('active')
        if current and current['id'] != previous_id and current['conversation_id'] == chat and current['status'] in ('starting','running'): break
        page.wait_for_timeout(100)
    page.select_option('[data-project-chat-model]', alternate)
    page.select_option('[data-project-chat-effort]', 'high')
    active, live = wait_private(page, 'codex-win', chat, after_id=previous_id)
    check('成员专属页调用真实 CLI 文件和命令工具', live and bool(active.get('tool_events')))
    check('项目私聊真实使用所选 Luna / low', active['model'] == initial_model and active['reasoning_effort'] == 'low')
    saved = desktop.ipc(page, 'get_conversation', id=chat)
    project_turn = next(a for a in detail['workflows'][0]['attempts'] if a['agent_id'] == 'codex-win')
    check('专属私聊与同成员项目执行使用独立原生上下文', saved['sessions'][0]['native_session_id'] != project_turn['native_thread_id'])
    page.screenshot(path=str(ROOT / 'artifacts/live-project-codex-tools.png'))
    select_team(page, room)
    page.wait_for_timeout(500)
    updated = desktop.ipc(page, 'get_conversation', id=room)
    broadcast = [m for m in updated['project_team_messages'] if public in m['content']]
    check('私聊真实广播送达团队一次', len(broadcast) == 1)
    row = page.locator(f'[data-team-message-id="{broadcast[0]["id"]}"]')
    expect(row).to_be_visible()
    check('广播是独立头像气泡，不在折叠协作卡中', row.locator('.avatar').count() == 1 and row.locator('.bubble').count() == 1 and row.evaluate("node => !node.closest('.workflow-card')"))
    check('群聊未泄漏私聊请求和私有标记', all(private not in m['content'] for m in updated['project_team_messages']) and all(private not in m['content'] for m in updated['messages']))
    page.screenshot(path=str(ROOT / 'artifacts/live-project-team-broadcast.png'))
    select_agent(page, 'codex-win')
    page.wait_for_timeout(250)
    page.fill('[data-project-agent-chat-input]', '无需工具，请仅回复：第二轮参数验证成功。')
    page.locator('[data-project-agent-chat-input]').press('Enter')
    second, _ = wait_private(page, 'codex-win', chat, after_id=active['id'])
    check(('模型与强度切换' if alternate != initial_model else '实时强度切换') + '应用于当前会话下一条真实请求', second['id'] != active['id'] and second['model'] == alternate and second['reasoning_effort'] == 'high')
    page.locator(f'[data-project-agent-chat-share="{second["assistant_message_id"]}"]').click()
    first_share = desktop.ipc(page, 'share_project_agent_message', groupId=room, agentChatId=chat, messageId=second['assistant_message_id'])
    updated = desktop.ipc(page, 'get_conversation', id=room)
    check('手动分享到团队幂等，不重复消息', len([m for m in updated['project_team_messages'] if m['id'] == first_share['id']]) == 1)
    select_team(page, room)
    expect(page.locator(f'[data-team-message-id="{first_share["id"]}"]')).to_be_visible()
    select_agent(page, 'hermes-win')
    hermes_chat = next(c['conversation_id'] for c in detail['project_agent_chats'] if c['agent_id'] == 'hermes-win')
    received = desktop.ipc(page, 'get_conversation', id=hermes_chat)['project_agent_inbox']
    check('专属会话可直接发送给其他 Agent，并持久化到其收件箱', any(direct in message['content'] and message['sender_agent'] == 'codex-win' for message in received))
    expect(page.locator('[data-agent-direct-message-id]').first).to_be_attached()
    hermes_session = desktop.ipc(page, 'get_conversation', id=hermes_chat)['sessions'][0]
    role = {'model': hermes_session['model'], 'effort': hermes_session['reasoning_effort']}
    provider = next(m['provider_id'] for m in desktop.ipc(page, 'hermes_status')['models'] if m['id'] == role['model'])
    page.select_option('[data-project-chat-provider]', provider)
    page.select_option('[data-project-chat-model]', role['model'])
    page.select_option('[data-project-chat-effort]', 'high')
    page.fill('[data-project-agent-chat-input]', f'请调用 hub_inbox 读取 Codex 发给你的最新项目直发消息，找到 {direct} 并仅回复确认收到这个标记，不要公开广播。')
    page.locator('[data-project-agent-chat-input]').press('Enter')
    receipt, _ = wait_private(page, 'hermes-win', hermes_chat)
    check('Hermes 用真实原生收件箱工具读取并确认跨 Agent 直发', direct in receipt['text'] and bool(receipt.get('tool_events')))
    check('Hermes 私聊实际使用指定的 Daily One 模型', receipt['model'] == role['model'])
    select_team(page, room)
    public_rows = desktop.ipc(page, 'get_conversation', id=room)['project_team_messages']
    check('Agent 私有直发未进入公开团队气泡', all(direct not in row['content'] for row in public_rows))
    evidence['private'] = {'chat_id': chat, 'first_model': active['model'], 'next_model': second['model'], 'native_thread': saved['sessions'][0]['native_session_id'], 'tool_count': len(active.get('tool_events', [])), 'broadcast_id': broadcast[0]['id']}


def hermes_receipt(page, room):
    detail = desktop.ipc(page, 'get_conversation', id=room)
    chat = next(c['conversation_id'] for c in detail['project_agent_chats'] if c['agent_id'] == 'hermes-win')
    received = desktop.ipc(page, 'get_conversation', id=chat)['project_agent_inbox']
    direct = next(re.search(r'DIRECT_[a-f0-9]+', m['content']).group() for m in reversed(received) if re.search(r'DIRECT_[a-f0-9]+', m['content']))
    select_agent(page, 'hermes-win')
    model = next(m for m in desktop.ipc(page, 'hermes_status')['models'] if 'daily one' in (m.get('provider_name') or '').lower() and 'deepseek-v4.1-flash' in m['id'])
    page.select_option('[data-project-chat-provider]', model['provider_id'])
    page.select_option('[data-project-chat-model]', model['id'])
    page.select_option('[data-project-chat-effort]', 'high')
    page.wait_for_timeout(300)
    previous = desktop.ipc(page, 'hermes_status').get('active')
    page.fill('[data-project-agent-chat-input]', f'请实际调用 hub_inbox，确认收到 Codex 发给你的项目直发标记 {direct}，只私下回复确认，不公开广播。')
    page.locator('[data-project-agent-chat-input]').press('Enter')
    receipt, _ = wait_private(page, 'hermes-win', chat, after_id=previous['id'] if previous else None)
    check('Hermes 真实工具读取跨 Agent 项目直发', direct in receipt['text'] and bool(receipt.get('tool_events')))
    check('Hermes 实际模型记录保留 Daily One 提供商身份', receipt['model'] == model['id'] and receipt['reasoning_effort'] == 'high')
    select_team(page, room)
    public = desktop.ipc(page, 'get_conversation', id=room)['project_team_messages']
    check('Hermes 读取的私有直发没有泄漏至团队', all(direct not in m['content'] for m in public))
    select_agent(page, 'hermes-win')
    page.screenshot(path=str(ROOT / 'artifacts/live-project-hermes-inbox.png'))
    evidence['hermes_receipt'] = {'model': receipt['model'], 'run': receipt['id'], 'tools': len(receipt.get('tool_events', []))}


def live_display(page, room):
    select_agent(page, 'dsh-win')
    status = desktop.ipc(page, 'dsh_status')
    model = next(m for m in status['models'] if m.get('provider_id') == 'command-code-daily')
    if page.locator('[data-project-chat-provider]').count(): page.select_option('[data-project-chat-provider]', model['provider_id'])
    page.select_option('[data-project-chat-model]', model['id'])
    page.select_option('[data-project-chat-effort]', 'high')
    page.wait_for_timeout(300)
    chat = next(c['conversation_id'] for c in desktop.ipc(page, 'get_conversation', id=room)['project_agent_chats'] if c['agent_id'] == 'dsh-win')
    previous = status.get('active')
    page.fill('[data-project-agent-chat-input]', '本轮隔离验收：实际读取 calc.py、cube.py、quad.py 和 check.py。运行 check.py，并另外用命令验证平方/立方/四次方对负整数、零、小数的结果与返回类型，共9例；检查有无副作用。只读，不修改文件、不广播。用中文简短报告实际结果和工具。')
    page.locator('[data-project-agent-chat-input]').press('Enter')
    samples=set(); captured=False; deadline=time.monotonic()+480
    while time.monotonic()<deadline:
        active=desktop.ipc(page, 'dsh_status').get('active')
        if active and active['conversation_id']==chat and active['id'] != (previous['id'] if previous else None):
            if active.get('thought'): samples.add(len(active['thought']))
            if active['status'] in ('starting','running') and active.get('thought') and not captured:
                page.screenshot(path=str(ROOT/'artifacts/live-agent-transcript-stream.png'));captured=True
            if active['status'] not in ('starting','running','cancelling'):
                check('DSH 原生文件和命令验证完成', active['status']=='completed' and bool(active.get('tool_events')))
                break
        page.wait_for_timeout(250)
    else: raise TimeoutError('DSH 会话显示验收超时')
    project=desktop.ipc(page, 'get_conversation', id=room)['project']
    result=subprocess.run(['python','-B','-c','from calc import square; from cube import cube; from quad import quad;\nfor fn,power in [(square,2),(cube,3),(quad,4)]:\n for value in [-2,0,2.5]:\n  answer=fn(value); assert answer==value**power; assert type(answer)==type(value)'],cwd=project['root'],capture_output=True,timeout=20)
    check('外部独立验证9个数值与返回类型用例通过', result.returncode==0)
    check('思考在原生运行结束之前显示', bool(samples) and captured)
    check('项目与私聊输出位于同一个滚动区', page.locator('.project-agent-chat-messages').count()==1 and page.locator('.project-agent-chat-messages [data-attempt-id]').count()>0 and page.locator('.project-agent-chat-messages [data-message-id]').count()>0)
    check('专属页不再有独立执行记录框或回复气泡', not page.locator('[data-project-history],.project-agent-room-activity,.bubble').count())
    check('完成的思考默认收起且正文保留', not page.locator(f'[data-message-id="{active["assistant_message_id"]}"] .thought').evaluate('node=>node.open') and bool(active['text']))
    page.wait_for_function("() => {const n=document.querySelector('.project-agent-chat-messages');return n && n.scrollHeight-n.scrollTop-n.clientHeight<3 && n.querySelector('.agent-jump-latest').hidden;}", timeout=5000)
    check('正文流式增长和思考收起后仍自动跟随最新', True)
    page.screenshot(path=str(ROOT/'artifacts/live-agent-transcript-complete.png'))
    page.set_viewport_size({'width':980,'height':760})
    page.wait_for_timeout(150)
    composer=page.locator('[data-project-agent-chat-form]').bounding_box()
    check('窄窗口没有横向溢出且输入框可见', page.evaluate('document.documentElement.scrollWidth <= innerWidth') and composer and composer['y']+composer['height'] <= page.evaluate('innerHeight'))
    page.screenshot(path=str(ROOT/'artifacts/live-agent-transcript-narrow.png'))
    evidence['display']={'model':active['model'],'stream_samples':len(samples),'tools':len(active.get('tool_events',[]))}


def automatic_round(page, room):
    configure_agents(page, room, json.loads(latest(page, room)['roles']))
    select_team(page, room)
    before = latest(page, room)['id']
    page.fill('#message-input', '本轮新目标：新增 quad.py，提供 quad(value) 返回四次方，支持负数与零。只改 quad.py，只分1个实现任务，不要把验收拆成实现任务。Hermes负责规划，DSH负责实现，Codex负责验收。请通过 hub_broadcast 在团队同步简短进展，实现完成后用 hub_message 向 Codex 交接。验收实际运行函数，不需要用户额外操作。')
    page.locator('#message-input').press('Enter')
    page.wait_for_timeout(400)
    done = wait_job(page, room, lambda j: j['id'] != before and j['status'] == 'completed', capture=True)
    check('群聊 Enter 发送后自动规划、实现、验收，无用户确认步骤', not page.locator('[data-confirm-project]').is_visible() and not page.locator('#role-form').is_visible())
    project = desktop.ipc(page, 'get_conversation', id=room)['project']
    result = subprocess.run(['python','-B','-c','from quad import quad; assert quad(3)==81; assert quad(-4)==256; assert quad(0)==0'], cwd=project['root'], capture_output=True, timeout=20)
    check('新一轮仅实现当前新目标，没有被旧讨论带偏', result.returncode == 0 and bool(done['tasks']) and all('quad.py' in task['files'] for task in done['tasks']))
    check('群聊文字可以改变规划和验收成员', any(a['stage']=='plan' and a['agent_id']=='hermes-win' for a in done['attempts']) and any(a['stage']=='review' and a['agent_id']=='codex-win' for a in done['attempts']))
    check('新一轮所有模型遵守无 Sol 的验收设置', all('sol' not in (attempt['model'] or '').lower() for attempt in done['attempts']))
    messages = done.get('team_messages', [])
    check('真实 Agent 在团队发布了项目协作消息', bool(messages))
    for message in messages:
        expect(page.locator(f'[data-team-message-id="{message["id"]}"]')).to_be_visible()
        check('同一团队消息只显示一次：' + message['id'][:8], page.locator(f'[data-team-message-id="{message["id"]}"]').count() == 1)
    page.screenshot(path=str(ROOT/'artifacts/live-project-automatic-team.png'))
    evidence['automatic'] = {'workflow':done['id'],'task_agents':[task['agent_id'] for task in done['tasks']],'public_senders':list({message.get('sender_agent') for message in messages}),'roles':json.loads(done['roles'])}


def partial_pause(page):
    root = Path(tempfile.mkdtemp(prefix='tongxi-team-fixture-'))
    (root / 'alpha.py').write_text('def square(value):\n    return value\n', encoding='utf-8')
    (root / 'beta.py').write_text('def cube(value):\n    return value\n', encoding='utf-8')
    project = desktop.ipc(page, 'register_project', name='单成员中断与恢复验收', root=str(root))
    room = groups.group(page, '单成员中断与恢复验收', ['codex-win', 'dsh-win', 'hermes-win'])
    desktop.ipc(page, 'bind_project', conversationId=room, projectId=project['id'])
    check('空项目目录无需人工建 Git 即可绑定', (root / '.git').is_dir())
    dsh = next(m['id'] for m in desktop.ipc(page, 'dsh_status')['models'] if m.get('provider_id') == 'command-code-daily')
    hermes = next(m['id'] for m in desktop.ipc(page, 'hermes_status')['models'] if 'daily one' in (m.get('provider_name') or '').lower() and 'deepseek-v4.1-flash' in m['id'])
    luna = os.environ.get('AGENT_HUB_TEST_LUNA', 'gpt-6-luna')
    if 'luna' not in luna.lower(): raise ValueError('此验收只允许 Luna 模型')
    roles = {'plan': {'agent': 'codex-win', 'model': luna, 'effort': 'low'}, 'implement': {'agent': 'dsh-win', 'model': dsh, 'effort': 'high'}, 'review': {'agent': 'hermes-win', 'model': hermes, 'effort': 'high'}}
    configure_agents(page, room, roles)
    ask = f'这是单成员中断恢复验收。严格分3项任务：1由DSH只改alpha.py，将square改为平方，但任务开始先运行一个等待20秒的命令再修改。2由Codex只改beta.py，将cube改为立方，此任务与1独立。3由DSH新建gamma.py，函数describe(value)调用alpha.square并返回结果，只依赖任务1（JSON depends_on 为 [0]，序号从0开始）。只改这3个文件。任务2必须使用{luna}/low。请遵守这些任务分配。'
    desktop.ipc(page, 'plan_project', room=room, message=ask, content=None, roles=roles)
    plan = wait_job(page, room, lambda j: j.get('plan') and any(a['stage'] == 'plan' and a['status'] == 'completed' for a in j['attempts']))
    check('真实规划按需求为独立任务选择不同 Agent', len(plan['tasks']) == 3 and [t['agent_id'] for t in plan['tasks']] == ['dsh-win', 'codex-win', 'dsh-win'])
    check('真实规划保留独立任务及明确依赖', plan['tasks'][1]['depends_on'] == [] and plan['tasks'][2]['depends_on'] == [0])
    choices = [{'position': t['position'], 'agent_id': t['agent_id'], 'model': luna if t['agent_id'] == 'codex-win' else dsh, 'effort': 'low' if t['agent_id'] == 'codex-win' else 'high'} for t in plan['tasks']]
    desktop.ipc(page, 'confirm_project', workflowId=plan['id'], tasks=choices)
    wait_job(page, room, lambda j: any(a['agent_id'] == 'dsh-win' and a['stage'] == 'implement' and a['status'] in ('starting', 'running') for a in j['attempts']))
    select_team(page, room)
    select_agent(page, 'dsh-win')
    expect(page.locator('[data-stop-project-agent="dsh-win"]')).to_be_visible()
    page.locator('[data-stop-project-agent="dsh-win"]').click()
    paused = wait_job(page, room, lambda j: j['status'] == 'interrupted', allow_pause=True)
    check('只中断 DSH，独立 Codex 任务仍完成并合并', paused['tasks'][0]['status'] == 'interrupted' and paused['tasks'][1]['status'] == 'completed')
    check('依赖被中断任务的后续工作保持等待', paused['tasks'][2]['status'] == 'queued')
    check('中断的工作树保留供恢复', bool(paused['tasks'][0]['worktree']) and Path(paused['tasks'][0]['worktree']).is_dir())
    select_team(page, room)
    expect(page.locator('.project-team-message.system')).to_be_visible()
    page.screenshot(path=str(ROOT / 'artifacts/live-project-single-agent-pause.png'))
    count = len([a for a in paused['attempts'] if a['stage'] == 'plan'])
    page.fill('#message-input', '继续协作，执行由DSH，验收由Hermes')
    page.locator('#message-input').press('Enter')
    done = wait_job(page, room, lambda j: j['status'] == 'completed', capture=True)
    check('恢复沿用原规划，并由 Hermes 完成最终验收', len([a for a in done['attempts'] if a['stage'] == 'plan']) == count and all(t['status'] == 'completed' for t in done['tasks']) and any(a['stage'] == 'review' and a['agent_id'] == 'hermes-win' for a in done['attempts']))
    verified = subprocess.run(['python', '-c', 'import alpha,beta,gamma; assert alpha.square(-4)==16; assert beta.cube(-3)==-27; assert gamma.describe(5)==25'], cwd=root, capture_output=True, timeout=20)
    check('中断恢复后的最终项目通过3个实际功能断言', verified.returncode == 0)
    check('恢复保留原任务分支，而非重新创建工作树', done['tasks'][0]['branch'] == paused['tasks'][0]['branch'])
    check('中断恢复验收未调用 Sol', all('sol' not in (a.get('model') or '').lower() for a in done['attempts']))
    evidence['pause'] = {'room': room, 'root': str(root), 'workflow': done['id'], 'paused_task_statuses': [t['status'] for t in paused['tasks']], 'plan_attempts': count}


def run(only):
    previous = json.loads((ROOT / 'artifacts/project-roles-ui-verification.json').read_text(encoding='utf-8'))
    desktop.DATA = Path(previous['test_data_directory'])
    desktop.EXE = Path(os.environ.get('AGENT_HUB_EXE', str(ROOT / 'src-tauri/target/debug/local-agent-hub.exe')))
    with sqlite3.connect(desktop.DATA / 'hub.db') as db:
        room = db.execute('SELECT conversation_id FROM workflows ORDER BY created_at LIMIT 1').fetchone()[0]
    proc = None
    success = False
    try:
        with sync_playwright() as p:
            proc, endpoint = desktop.launch()
            browser = p.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on('pageerror', lambda error: errors.append(str(error)))
            for agent in ('codex', 'dsh', 'hermes'):
                desktop.ipc(page, 'connect_' + agent)
                prior.wait_connection(page, agent)
            select_team(page, room)
            if only in ('all', 'resume'):
                resume_failed(page, room)
                if only == 'all': automatic_round(page, room)
            if only in ('all', 'private'):
                private_tools(page, room)
            if only == 'display':
                live_display(page, room)
            if only == 'hermes':
                hermes_receipt(page, room)
            if only in ('all', 'pause'):
                partial_pause(page)
            check('真实窗口运行期间无 JavaScript 错误', not errors)
            success = True
    finally:
        if proc and proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=10)
        destination = REPORT if only == 'all' else REPORT.with_name(REPORT.stem + '-' + only + '.json')
        destination.write_text(json.dumps({'success': success, 'only': only, 'passed': len(checks), 'checks': checks, 'javascript_errors': errors, 'data': str(desktop.DATA), 'executable_sha256': hashlib.sha256(desktop.EXE.read_bytes()).hexdigest(), 'evidence': evidence}, ensure_ascii=False, indent=2), encoding='utf-8')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--only', choices=['all', 'resume', 'private', 'hermes', 'pause', 'display'], default='all')
    run(parser.parse_args().only)
