"""Real packaged group discussion: native public context, isolation and recovery.

Uses isolated test databases, UI/CDP, and read-only native evidence for sessions
created here. Never prints original private history, config or raw RPC errors.
"""
import json
from pathlib import Path
import sqlite3
import time
import uuid
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_stage3 as prior
import verify_stage4 as dsh_checks
import verify_stage5 as albion_checks
import probe_albion as probe

checks=[]; errors=[]; evidence=[]
def check(name,condition=True):
    if not condition: raise AssertionError(name)
    checks.append(name); print('PASS '+name,flush=True)

def reject(page,command,**args):
    return page.evaluate('async ([command,args]) => { try { await window.__TAURI_INTERNALS__.invoke(command,args); return false; } catch { return true; } }',[command,args])

def migration():
    report=json.loads((desktop.ARTIFACTS/'stage5-verification.json').read_text(encoding='utf-8'))
    source=Path(report['test_data_directory'])/'hub.db'; desktop.DATA.mkdir(parents=True)
    with sqlite3.connect('file:'+source.as_posix()+'?mode=ro',uri=True) as old,sqlite3.connect(desktop.DATA/'hub.db') as new:
        if old.execute('PRAGMA user_version').fetchone()[0]!=5: raise AssertionError('Historical v5 migration fixture is unavailable')
        old.backup(new)
        return {table:([row[1] for row in old.execute('PRAGMA table_info('+table+')')],old.execute('SELECT * FROM '+table+' ORDER BY rowid').fetchall()) for table in ['conversations','members','sessions','messages','runs']}

def validate_migration(old):
    with sqlite3.connect(desktop.DATA/'hub.db') as db:
        check('actual v5 upgrade preserves every old message setting and native mapping',db.execute('PRAGMA user_version').fetchone()[0]==6 and not db.execute('PRAGMA foreign_key_check').fetchall() and all(db.execute('SELECT '+','.join(cols)+' FROM '+table+' ORDER BY rowid').fetchall()==rows for table,(cols,rows) in old.items()))
    backups=list((desktop.DATA/'backups').glob('hub-schema-v5-*.db'))
    check('migration keeps a complete readable v5 backup',len(backups)==1)
    with sqlite3.connect('file:'+backups[0].as_posix()+'?mode=ro',uri=True) as db:
        check('migration backup retains the original schema and records',db.execute('PRAGMA user_version').fetchone()[0]==5 and all(db.execute('SELECT * FROM '+table+' ORDER BY rowid').fetchall()==rows for table,(_,rows) in old.items()))

def group(page,title,members):
    page.locator('#new-conversation').click();page.locator('[data-kind="group"]').click()
    page.locator('#create-form [name="title"]').fill(title)
    for agent in ['hermes-win','codex-win','dsh-win','albion-wsl']:
        page.locator('#create-form input[value="'+agent+'"]').set_checked(agent in members)
    page.locator('#create-form [type="submit"]').click()
    expect(page.locator('.chat-heading h1')).to_have_text(title)
    return page.evaluate("localStorage.getItem('hub.selected')")

def job(page,room): return desktop.ipc(page,'get_conversation',id=room)['discussions'][0]
def wait_job(page,room,predicate,timeout=360):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        current=job(page,room)
        if predicate(current): return current
        page.wait_for_timeout(100)
    raise AssertionError('Discussion state timeout')

def wait_done(page,room,status='completed'):
    current=wait_job(page,room,lambda j:j['status'] in ['completed','interrupted','failed'])
    check('discussion reaches '+status,current['status']==status)
    return current

def live_settings(page,agent,model,effort):
    page.locator('#live-agent').select_option(agent)
    expect(page.locator('#live-agent')).to_have_value(agent)
    page.locator('#message-input').click()
    page.locator('#live-model').select_option(model or '')
    page.locator('#live-effort').select_option(effort or '')
    page.locator('#message-input').click()
    room=page.evaluate("localStorage.getItem('hub.selected')")
    deadline=time.monotonic()+10
    while time.monotonic()<deadline:
        session=next(s for s in desktop.ipc(page,'get_conversation',id=room)['sessions'] if s['agent_id']==agent)
        if session['model']==(model or None) and session['reasoning_effort']==(effort or None): return
        page.wait_for_timeout(50)
    raise AssertionError('Group live settings failed to persist')

def send(page,room,content,rounds=1):
    page.locator('#discussion-rounds').select_option(str(rounds))
    page.locator('#message-input').fill(content)
    expect(page.locator('#send-discussion')).to_be_enabled(timeout=15000)
    page.locator('#send-discussion').click()
    deadline=time.monotonic()+15
    while time.monotonic()<deadline:
        jobs=desktop.ipc(page,'get_conversation',id=room)['discussions']
        if jobs and jobs[0]['status'] in ['running','cancelling']: return jobs[0]
        page.wait_for_timeout(50)
    raise AssertionError('Group send did not start')

def group_prompts(agent,thread):
    if agent=='hermes-win':
        with sqlite3.connect('file:'+(desktop.DATA/'hermes-native/sessions.db').as_posix()+'?mode=ro',uri=True) as db:
            values=[row[0] for row in db.execute("SELECT content FROM messages WHERE session_id=? AND role='user' ORDER BY id",[thread])]
    elif agent=='dsh-win':
        values=[]
        for path in (desktop.DATA/'dsh-native/sessions').rglob('*.jsonl'):
            if thread not in str(path): continue
            for line in path.read_text(encoding='utf-8').splitlines():
                event=json.loads(line)
                if event.get('type')=='user/message':
                    content=event['data']['content']; values.append(content if isinstance(content,str) else ''.join(part.get('text','') for part in content if isinstance(part,dict)))
    else: raise AssertionError('Unsupported native evidence adapter')
    return [json.loads(value.split('公开记录（JSON）：\n',1)[1]) for value in values if '公开记录（JSON）：\n' in value]

def response_map(page,room):
    return {m['id']:m['content'] for m in desktop.ipc(page,'get_conversation',id=room)['messages']}

def run():
    proc=None; success=False; old=migration(); before=probe.profile_hashes(); gateway=albion_checks.gateway_pids()
    try:
        with sync_playwright() as playwright:
            proc,endpoint=desktop.launch(); browser=playwright.chromium.connect_over_cdp(endpoint); page=desktop.page_for(browser)
            page.on('pageerror',lambda error:errors.append(str(error)))
            validate_migration(old)
            room=group(page,'群聊真实两轮验收',['hermes-win','dsh-win'])
            expect(page.locator('#send-discussion')).to_be_disabled()
            page.locator('#connect-discussion').click()
            hermes=prior.wait_connection(page,'hermes'); dsh=prior.wait_connection(page,'dsh')
            check('group UI connects both selected native members',bool(hermes['models']) and len(dsh['models'])==2)
            first_route,second_route=dsh['models'][0]['id'],dsh['models'][1]['id']
            live_settings(page,'hermes-win',None,'low'); live_settings(page,'dsh-win',first_route,'off')
            private=prior.new_private(page,'dsh-win','私聊保密验收')
            page.locator('#live-effort').select_option('off'); page.locator('#message-input').click()
            secret='PRIVATE_ONLY_'+uuid.uuid4().hex[:10]
            user=prior.send(page,'dsh','这是私聊隔离验收，请记住标记 '+secret+'，仅回复已记住。')
            private_run=prior.wait_run(page,'dsh',user)
            desktop.choose(page,room)
            marker='GROUP_PUBLIC_'+uuid.uuid4().hex[:10]
            content=f'请协作设计一个本地待办应用，只讨论，不写代码。每次发言不超过100字，并包含验收标记 {marker}。Hermes第一轮提出SQLite保存；DSH第一轮回应Hermes的存储建议并补充输入校验。第二轮各自引用另一位的具体建议，给出一个验收步骤。'
            initial=send(page,room,content,2)
            live_settings(page,'hermes-win',None,'none')
            wait_job(page,room,lambda j:len(j['turns'])>=2)
            live_settings(page,'dsh-win',second_route,'off')
            finished=wait_done(page,room)
            turns=finished['turns']; text=response_map(page,room)
            check('two real members exchange four ordered replies for one user message',[(r['agent_id'],r['round']) for r in turns]==[('hermes-win',1),('dsh-win',1),('hermes-win',2),('dsh-win',2)] and len({r['user_message_id'] for r in turns})==1 and all(text[r['assistant_message_id']].strip() for r in turns))
            check('group turns freeze current settings and apply live changes next round',turns[0]['reasoning_effort']=='low' and turns[2]['reasoning_effort']=='none' and turns[1]['model']==first_route and turns[3]['model']==second_route)
            hermes_inputs=group_prompts('hermes-win',turns[0]['native_thread_id']); dsh_inputs=group_prompts('dsh-win',turns[1]['native_thread_id'])
            check('DSH actually receives the prior Hermes public reply',any(any(m.get('message_id')==turns[0]['assistant_message_id'] and m['content']==text[turns[0]['assistant_message_id']] for m in prompt) for prompt in dsh_inputs))
            check('Hermes next round actually receives the prior DSH public reply',any(any(m.get('message_id')==turns[1]['assistant_message_id'] and m['content']==text[turns[1]['assistant_message_id']] for m in prompt) for prompt in hermes_inputs))
            check('group native prompts contain no private marker or private session',secret not in json.dumps(hermes_inputs+dsh_inputs,ensure_ascii=False) and turns[1]['native_thread_id']!=private_run['native_thread_id'] and all(secret not in text[r['assistant_message_id']] for r in turns))
            native=dsh_checks.native_dsh_evidence(turns[1]); evidence.append({'group_dsh_contexts':native['contexts'],'tool_counts':native['tool_counts']})
            check('group DSH uses both exact DS v4.1 routes without tools',{'command-code-daily','command-code-daily-2'}<={c['provider'] for c in native['contexts']} and all(c['model']=='deepseek/deepseek-v4.1-flash' and c['reasoningEffort']=='off' for c in native['contexts']) and all(count==0 for count in native['tool_counts']))
            check('private-session settings stay independent of group switches',desktop.ipc(page,'get_conversation',id=private)['sessions'][0]['model'] is None)
            same=desktop.ipc(page,'start_discussion',conversationId=room,messageId=initial['user_message_id'],content=content,participants=finished['participants'],rounds=2)
            check('repeated group request is idempotent and cannot enter private adapter',same['id']==finished['id'] and len(same['turns'])==4 and reject(page,'send_hermes_message',conversationId=room,messageId=initial['user_message_id'],content=content))
            page.screenshot(path=str(desktop.ARTIFACTS/'stage6-group-discussion.png'))

            coding=group(page,'Codex 群讨论验收',['codex-win','dsh-win'])
            page.locator('#connect-discussion').click(); codex=prior.wait_connection(page,'codex')
            live_settings(page,'codex-win',None,'low'); live_settings(page,'dsh-win',None,'off')
            send(page,coding,'Codex请提出这个本地待办程序的一条实现建议，DSH请回应上一条建议并补充一条边界检查。每人不超过70字。')
            coded=wait_done(page,coding); codex_turn=coded['turns'][0]
            prior.verify_codex_context(codex_turn)
            check('Codex participates with a real independent group thread',codex_turn['native_thread_id'] and codex_turn['status']=='completed' and codex_turn['reasoning_effort']=='low')

            invited=group(page,'阿尔比恩受邀群讨论',['hermes-win','albion-wsl'])
            check('Albion is not selected automatically in a new group',not page.locator('[data-discussion-member="albion-wsl"]').is_checked() and page.locator('#send-discussion').is_disabled())
            page.locator('[data-discussion-member="albion-wsl"]').check(); page.locator('#connect-discussion').click(); prior.wait_connection(page,'albion')
            live_settings(page,'hermes-win',None,'none'); live_settings(page,'albion-wsl',None,'none')
            send(page,invited,'我准备睡觉了，项目明天继续。Hermes先简短整理明天的一件开发事项，阿尔比恩回应这条计划并对我说晚安，每人不超过60字。')
            companion=wait_done(page,invited); bodies=response_map(page,invited); info=albion_checks.native_info()
            check('explicitly invited Albion replies under her original identity in her own group session',companion['turns'][1]['agent_id']=='albion-wsl' and bodies[companion['turns'][1]['assistant_message_id']].strip() and info['identity_matched'] and info['relationship_matched'])
            check('group visible Albion reply filters voice metadata','<|ACT:' not in bodies[companion['turns'][1]['assistant_message_id']])

            desktop.choose(page,room); draft='群聊中文草稿保留，明天再讨论。'; page.locator('#message-input').fill(draft)
            mapping={s['agent_id']:s['native_session_id'] for s in desktop.ipc(page,'get_conversation',id=room)['sessions']}
            pid=info['pid']; proc.terminate();proc.wait(timeout=10);proc=None;browser.close()
            check('app exit also cleans up its group Albion Linux child',albion_checks.wait_linux_exit(pid))
            proc,endpoint=desktop.launch();browser=playwright.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser);page.on('pageerror',lambda error:errors.append(str(error)))
            expect(page.locator('#message-input')).to_have_value(draft)
            restored=desktop.ipc(page,'get_conversation',id=room)
            check('restart retains discussion history draft settings and separate native sessions',restored['discussions'][0]['status']=='completed' and {s['agent_id']:s['native_session_id'] for s in restored['sessions']}==mapping)
            page.locator('#connect-discussion').click();prior.wait_connection(page,'hermes');prior.wait_connection(page,'dsh')
            send(page,room,'继续刚才这个群里的讨论，请简短说出我们讨论的程序和之前的验收标记。每人不超过70字。')
            resumed=wait_done(page,room);bodies=response_map(page,room)
            check('real group continuation loads the same native sessions and public context',all(r['native_thread_id']==mapping[r['agent_id']] for r in resumed['turns']) and all(marker in bodies[r['assistant_message_id']] for r in resumed['turns']))

            long='取消验收，请逐行输出编号及一个本地待办应用的测试用例，总共1000行。不要执行工具。'
            send(page,room,long,3)
            running=wait_job(page,room,lambda j:bool(j['turns']) and bool(j['turns'][0]['native_turn_id']))
            check('active discussion protects roster archive and deletion',reject(page,'archive_conversation',id=room,archived=True) and reject(page,'delete_conversation',id=room) and reject(page,'update_group_members',id=room,members=['hermes-win','dsh-win']))
            page.locator('#cancel-discussion').click();stopped=wait_done(page,room,'interrupted')
            page.wait_for_timeout(500)
            check('stopping real group generation prevents every later member and round',len(stopped['turns'])==1 and len(job(page,room)['turns'])==1 and not desktop.ipc(page,'discussion_status'))

            desktop.ipc(page,'disconnect_hermes');desktop.ipc(page,'set_session_settings',id=room,agentId='hermes-win',model='__HUB_INVALID_TEST_MODEL__',reasoningEffort=None);desktop.ipc(page,'connect_hermes')
            send(page,room,'无效模型设置的失败状态验收。')
            failed=wait_done(page,room,'failed')
            check('failed admission has an honest failed state and does not fan out',len(failed['turns'])==1 and failed['turns'][0]['status']=='failed' and len(response_map(page,room)[failed['turns'][0]['assistant_message_id']])==0)
            desktop.ipc(page,'set_session_settings',id=room,agentId='hermes-win',model=None,reasoningEffort='none')

            send(page,room,long,2);crashed=wait_job(page,room,lambda j:bool(j['turns']) and bool(j['turns'][0]['native_turn_id']))
            proc.terminate();proc.wait(timeout=10);proc=None;browser.close()
            proc,endpoint=desktop.launch();browser=playwright.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser);page.on('pageerror',lambda error:errors.append(str(error)))
            recovered=job(page,room);page.wait_for_timeout(500)
            check('crashed discussion restores as interrupted and never replays',recovered['id']==crashed['id'] and recovered['status']=='interrupted' and len(recovered['turns'])==1 and recovered['turns'][0]['status']=='interrupted' and not desktop.ipc(page,'discussion_status'))
            page.locator('#connect-discussion').click();prior.wait_connection(page,'hermes');prior.wait_connection(page,'dsh')
            send(page,room,'恢复验收，每人简短确认可以继续讨论，不超过20字。')
            fresh=wait_done(page,room)
            check('fresh discussion succeeds after cancel failure and crash recovery',len(fresh['turns'])==2 and fresh['id']!=crashed['id'])
            check('original Albion profile and voice gateway remain unchanged',probe.profile_hashes()==before and albion_checks.gateway_pids()==gateway)
            check('no JavaScript errors in real group flows',not errors)
            browser.close();success=True
    finally:
        if proc and proc.poll() is None:proc.terminate();proc.wait(timeout=10)
        counts={}
        if (desktop.DATA/'hub.db').exists():
            with sqlite3.connect(desktop.DATA/'hub.db') as db:counts={row[0]:row[1] for row in db.execute('SELECT agent_id,COUNT(*) FROM runs WHERE discussion_id IS NOT NULL GROUP BY agent_id')}
        (desktop.ARTIFACTS/'stage6-verification.json').write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks,'javascript_errors':errors,'native_evidence':evidence,'group_run_record_counts':counts,'test_data_directory':str(desktop.DATA),'input_method':'WebView2 CDP UI and real Rust IPC; native evidence inspected read-only'},ensure_ascii=False,indent=2),encoding='utf-8')
    print(f'Stage 6: {len(checks)} checks',flush=True)

if __name__=='__main__':run()
