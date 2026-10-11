"""Real private clarification round trips in ordinary and project-member conversations.

Codex uses Luna; Hermes/DSH use configured DS v4.1 routes; Albion is excluded.
AGENT_HUB_QA_REUSE_DATA reuses completed ordinary-chat evidence without new model calls.
"""
import hashlib
import json
import os
from pathlib import Path
import tempfile
import time
import uuid
from playwright.sync_api import sync_playwright,expect
import smoke_desktop as desktop
import verify_stage3 as prior
import verify_stage6 as groups
from verify_project_v17 import module

ROOT=Path(__file__).resolve().parents[1]
checks=[];evidence=[];errors=[]
def check(label,condition=True):
    if not condition:raise AssertionError(label)
    checks.append(label);print('PASS '+label,flush=True)

def save(success=False):
    (ROOT/'artifacts/private-questions-live.json').write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks,'errors':errors,'data':str(desktop.DATA),'evidence':evidence,'sha256':hashlib.sha256(desktop.EXE.read_bytes()).hexdigest(),'excluded':['albion-wsl']},ensure_ascii=False,indent=2),encoding='utf-8')

def wait_question(page,chat,timeout=300):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        detail=desktop.ipc(page,'get_conversation',id=chat)
        found=next((q for q in detail.get('chat_questions',[]) if q['status']=='pending'),None)
        if found:return found
        failed=next((m for m in reversed(detail['messages']) if m['sender_id']!='user' and m['status']=='failed'),None)
        if failed:raise AssertionError('私聊在提问前失败')
        page.wait_for_timeout(200)
    raise TimeoutError('没有显示私聊问题')

def ask_and_answer(page,chat,agent,project=False):
    marker='PRIVATE_'+uuid.uuid4().hex[:8]
    field='[data-project-agent-chat-input]' if project else '#message-input'
    page.fill(field,f'私聊问答验收：请先向我询问“{marker}：采用选项A还是选项B？”，提供两个选项“选项A”“选项B”。使用同席提问工具或tongxi-question回复协议。提问后等待我的实际回答，再只用一句话确认选择结果。不要自主选择，不发团队广播，不用其他工具，不查询或保存记忆。')
    page.locator(field).press('Enter')
    question=wait_question(page,chat)
    check(agent+(' 项目私聊' if project else ' 普通私聊')+'收到带选项的问题',len(question['options'])==2)
    expect(page.locator(f'[data-answer-chat-question="{question["id"]}"]')).to_be_visible(timeout=15000)
    expect(page.locator('#question-notice')).to_be_visible(timeout=15000)
    check('待答提示和问题下回答按钮可见')
    before=desktop.ipc(page,'get_conversation',id=chat)['sessions'][0].get('native_session_id')
    page.screenshot(path=str(ROOT/'artifacts'/(f'qa-{agent}-'+('project' if project else 'ordinary')+'.png')))
    page.locator(f'[data-answer-chat-question="{question["id"]}"]').click()
    expect(page.locator('#chat-question-option')).to_be_visible()
    expect(page.locator('#chat-question-option')).to_have_value('')
    expect(page.locator('#chat-question-text')).to_have_value('')
    page.locator('#modal-close').click()
    saved=next(q for q in desktop.ipc(page,'get_conversation',id=chat)['chat_questions'] if q['id']==question['id'])
    check('关闭回答窗口不会代答或取消问题',saved['status']=='pending' and saved['answer'] is None)
    page.locator(f'[data-answer-chat-question="{question["id"]}"]').click()
    page.locator('#chat-question-option').select_option(question['options'][1])
    expect(page.locator('#chat-question-text')).to_have_value(question['options'][1])
    page.locator('#chat-question-text').fill('选项B，采用第二方案。')
    page.locator('#chat-question-form button.primary').click()
    expect(page.locator('#modal')).not_to_be_visible()
    end=time.monotonic()+300;done=None
    while time.monotonic()<end:
        detail=desktop.ipc(page,'get_conversation',id=chat)
        row=next(q for q in detail['chat_questions'] if q['id']==question['id'])
        messages=[m for m in detail['messages'] if m['sender_id']==agent and (question['mode']=='native' or m['id']!=question['message_id']) and m['status'] in ['completed','succeeded']]
        if row['status']=='answered' and row['delivery']=='sent' and messages:
            done=messages[-1];break
        if row.get('error'):raise AssertionError(row['error'])
        page.wait_for_timeout(200)
    if not done:raise TimeoutError('回答后没有继续私聊')
    check('选项可改为自由文本并实际送回成员',row['answer']=='选项B，采用第二方案。' and ('B' in done['content'] or '第二' in done['content']))
    after=desktop.ipc(page,'get_conversation',id=chat)['sessions'][0].get('native_session_id')
    check('回答保持原生会话ID与私聊范围',bool(before) and before==after)
    evidence.append({'agent':agent,'project':project,'question':question['id'],'mode':question['mode'],'native_session_preserved':before==after,'answer_received':True})
    return marker

def run():
    desktop.EXE=Path(os.environ.get('AGENT_HUB_EXE',str(ROOT/'release/Agent Hub.exe')))
    reused=os.environ.get('AGENT_HUB_QA_REUSE_DATA')
    if reused:
        desktop.DATA=Path(reused)
        if not (desktop.DATA/'hub.db').is_file():raise ValueError('Reuse database is missing')
    proc=None;success=False
    try:
        with sync_playwright() as p:
            proc,endpoint=desktop.launch();browser=p.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser)
            page.on('pageerror',lambda e:errors.append(str(e)))
            for key in ['codex','hermes','dsh']:
                desktop.ipc(page,'connect_'+key);prior.wait_connection(page,key)
            rooms=desktop.ipc(page,'list_conversations',search='',archived=False)
            chat_ids={r['members'][0]:r['id'] for r in rooms if r['kind']=='direct'}
            models={}
            for agent in ['codex-win','hermes-win','dsh-win']:
                key={'codex-win':'codex','hermes-win':'hermes','dsh-win':'dsh'}[agent];catalog=desktop.ipc(page,key+'_status')['models']
                provider=os.environ.get('AGENT_HUB_QA_PROVIDER','daily one').lower()
                model=next(m for m in catalog if m['id']=='gpt-5.6-luna') if key=='codex' else next(m for m in catalog if provider in (m.get('provider_name') or '').lower() and 'v4.1' in m['id'])
                models[agent]=model['id'];desktop.ipc(page,'set_session_settings',id=chat_ids[agent],agentId=agent,model=model['id'],reasoningEffort='low')
            for agent in ['codex-win','hermes-win','dsh-win']:
                if reused:
                    saved=desktop.ipc(page,'get_conversation',id=chat_ids[agent])
                    row=next(q for q in saved['chat_questions'] if q['status']=='answered' and q['delivery']=='sent')
                    replies=[m for m in saved['messages'] if m['sender_id']==agent and m['id']!=row['message_id'] and m['status']=='completed']
                    check(agent+' 复用普通私聊真实问答证据',len(row['options'])==2 and 'B' in row['answer'] and bool(replies) and ('B' in replies[-1]['content'] or '第二' in replies[-1]['content']))
                    evidence.append({'agent':agent,'project':False,'question':row['id'],'mode':row['mode'],'answer_received':True,'reused_prior_run':True})
                else:
                    page.locator(f'[data-conversation="{chat_ids[agent]}"]').click();ask_and_answer(page,chat_ids[agent],agent)
            save()
            group=os.environ.get('AGENT_HUB_QA_REUSE_PROJECT')
            if not group:
                root=Path(tempfile.mkdtemp(prefix='tongxi-private-qa-project-'));(root/'README.md').write_text('Synthetic fixture only',encoding='utf-8')
                project=desktop.ipc(page,'register_project',name='私聊问答隔离项目',root=str(root))
                group=groups.group(page,'私聊问答隔离项目',['codex-win','hermes-win','dsh-win'])
                desktop.ipc(page,'bind_project',conversationId=group,projectId=project['id'])
            page.locator(f'[data-conversation="{group}"]').click();detail=desktop.ipc(page,'get_conversation',id=group)
            for chat in detail['project_agent_chats']:
                agent=chat['agent_id'];desktop.ipc(page,'set_session_settings',id=chat['conversation_id'],agentId=agent,model=models[agent],reasoningEffort='low')
                module(page,group,agent)
                saved=desktop.ipc(page,'get_conversation',id=chat['conversation_id'])
                existing=next((q for q in saved.get('chat_questions',[]) if q['status']=='answered' and q['delivery']=='sent'),None) if os.environ.get('AGENT_HUB_QA_REUSE_PROJECT') else None
                if existing:
                    replies=[m for m in saved['messages'] if m['sender_id']==agent and m['id']!=existing['message_id'] and m['status']=='completed']
                    check(agent+' 复用项目私聊真实问答证据',len(existing['options'])==2 and 'B' in existing['answer'] and bool(replies) and ('B' in replies[-1]['content'] or '第二' in replies[-1]['content']))
                    marker=existing['prompt'].split('：')[0]
                    evidence.append({'agent':agent,'project':True,'question':existing['id'],'mode':existing['mode'],'answer_received':True,'reused_prior_run':True})
                else:
                    marker=ask_and_answer(page,chat['conversation_id'],agent,True)
                for width in [980,1440]:
                    page.set_viewport_size({'width':width,'height':900})
                    check(str(width)+' 项目成员页无横向溢出',page.evaluate('document.documentElement.scrollWidth <= innerWidth + 1'))
                public=desktop.ipc(page,'get_conversation',id=group)
                check('项目私聊问题和回答不自动进入群聊',all(marker not in m['content'] for m in public['messages']+public['project_team_messages']) and not public.get('chat_questions'))
                save()
            check('界面无JavaScript错误',not errors and not desktop.errors);success=True
    finally:
        if proc and proc.poll() is None:proc.terminate();proc.wait(timeout=10)
        save(success)

if __name__=='__main__':run()
