"""Real packaged app: inline live settings and native DSH. Isolated data only."""
import json
from pathlib import Path
import sqlite3
import sys
import time
import uuid
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_stage3 as prior

checks=[]
errors=[]
def check(name, condition=True):
    if not condition: raise AssertionError(name)
    checks.append(name); print('PASS '+name,flush=True)

def settings(page, model, effort):
    page.locator('#live-model').select_option(model or '')
    page.locator('#live-effort').select_option(effort or '')
    id=page.evaluate("localStorage.getItem('hub.selected')")
    agent=desktop.ipc(page,'get_conversation',id=id)['conversation']['members'][0]
    deadline=time.monotonic()+10
    while time.monotonic()<deadline:
        detail=desktop.ipc(page,'get_conversation',id=id)
        saved=next(s for s in detail['sessions'] if s['agent_id']==agent)
        if saved['model']==(model or None) and saved['reasoning_effort']==(effort or None):
            expect(page.locator('#live-model')).to_have_value(model or '')
            expect(page.locator('#live-effort')).to_have_value(effort or '')
            page.wait_for_timeout(180)
            return
        page.wait_for_timeout(80)
    raise AssertionError('Inline settings did not persist')

def nonce(): return 'HUB4_'+uuid.uuid4().hex[:8]

def native_dsh_evidence(run):
    # Read only the newly created isolated native test session; no user history.
    contexts=[]; tools=[]
    for path in (desktop.DATA/'dsh-native'/'sessions').rglob('*'):
        if run['native_thread_id'] not in str(path) or not path.is_file() or path.suffix!='.jsonl': continue
        for line in path.read_text(encoding='utf-8').splitlines():
            event=json.loads(line)
            if event.get('type')=='request/header':
                header=event.get('data',{}).get('header',{})
                config=header.get('config',{})
                contexts.append({k:config.get(k) for k in ('provider','model','reasoningEffort')})
                tools.append(len(header.get('tools',[])))
    return {'contexts':contexts,'tool_counts':tools}

def validate_native_evidence():
    # May run after integration to inspect already-created test records only.
    report_path=desktop.ARTIFACTS/'stage4-verification.json'
    report=json.loads(report_path.read_text(encoding='utf-8'))
    if not report['success']: raise AssertionError('Integration must pass first')
    desktop.DATA=Path(report['test_data_directory'])
    with sqlite3.connect(desktop.DATA/'hub.db') as db:
        db.row_factory=sqlite3.Row
        runs=[dict(row) for row in db.execute("SELECT * FROM runs WHERE agent_id='dsh-win' AND status='completed' ORDER BY rowid")]
        report['real_run_counts']={row[0]:row[1] for row in db.execute('SELECT agent_id,COUNT(*) FROM runs GROUP BY agent_id')}
    evidence=[]
    seen=set()
    for run in runs:
        if run['native_thread_id'] in seen: continue
        seen.add(run['native_thread_id']); evidence.append(native_dsh_evidence(run))
    contexts=[c for e in evidence for c in e['contexts']]
    expected={('command-code-daily','off'),('command-code-daily-2','low'),('command-code-daily','low')}
    if not expected.issubset({(c['provider'],c['reasoningEffort']) for c in contexts}) or any(c['model']!='deepseek/deepseek-v4.1-flash' for c in contexts):
        raise AssertionError('Native DSH request headers differ from settings')
    if not contexts or any(count for e in evidence for count in e['tool_counts']): raise AssertionError('Unexpected native tools')
    report['native_dsh_evidence']=evidence
    name='native DSH request headers confirm both routes, off/low and no tools'
    if name not in report['checks']: report['checks'].append(name); report['passed']+=1
    report_path.write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf-8')
    print('PASS '+name,flush=True)

def run():
    proc=None; success=False; evidence=[]
    try:
        with sync_playwright() as playwright:
            proc, endpoint=desktop.launch(); browser=playwright.chromium.connect_over_cdp(endpoint); page=desktop.page_for(browser)
            page.on('pageerror', lambda error:errors.append(str(error)))
            check('model controls are inside the current conversation',page.locator('#live-settings #live-model').is_visible() and not page.locator('#modal').is_visible())
            # An unconnected member can edit without opening a settings dialog.
            page.locator('#live-agent').select_option('dsh-win')
            page.locator('#live-model').fill('pending-dsh-model'); page.locator('#live-model').blur()
            page.wait_for_timeout(200)
            page.locator('#live-effort').select_option('high'); page.wait_for_timeout(200)
            group=page.evaluate("localStorage.getItem('hub.selected')")
            saved=desktop.ipc(page,'get_conversation',id=group)
            check('inline group settings remain per member',next(s for s in saved['sessions'] if s['agent_id']=='dsh-win')['model']=='pending-dsh-model' and all(s['model'] is None for s in saved['sessions'] if s['agent_id']!='dsh-win'))
            codex=prior.new_private(page,'codex-win','实时切换 Codex')
            page.locator('#connect-codex').click(); catalog=prior.wait_connection(page,'codex')
            expect(page.locator('#live-model')).to_have_js_property('tagName','SELECT')
            luna=next(model for model in catalog['models'] if 'luna' in model['id'] and 'low' in model['efforts'])
            settings(page,luna['id'],'low')
            marker=nonce()
            user=prior.send(page,'codex',f'不调用工具。记住标记 {marker}，先输出标记，然后逐行输出10个编号句子，每句说“当前轮次参数保持不变”。')
            prior.wait_run(page,'codex',user,terminal=False)
            next_luna=next((model for model in catalog['models'] if 'luna' in model['id'] and model['id']!=luna['id']),luna)
            settings(page,next_luna['id'],'medium')
            first=prior.wait_run(page,'codex',user)
            check('live switch preserves the current Codex turn',first['model']==luna['id'] and first['reasoning_effort']=='low' and marker in first['text'])
            prior.verify_codex_context(first)
            second=prior.wait_run(page,'codex',prior.send(page,'codex','不调用工具。只回复刚才的验证标记。'))
            prior.verify_codex_context(second)
            check('the next Codex turn uses the selected Luna model and retains context',second['model']==next_luna['id'] and second['reasoning_effort']=='medium' and first['native_thread_id']==second['native_thread_id'] and marker in second['text'])
            # Hermes uses the same inline save path while its prompt runs.
            hermes=prior.new_private(page,'hermes-win','实时切换 Hermes')
            page.locator('#connect-hermes').click(); prior.wait_connection(page,'hermes')
            settings(page,None,'low')
            marker_h=nonce()
            user=prior.send(page,'hermes',f'不调用工具。记住 {marker_h}，只回复这个标记。')
            prior.wait_run(page,'hermes',user,terminal=False)
            settings(page,None,'none')
            first_h=prior.wait_run(page,'hermes',user)
            next_h=prior.wait_run(page,'hermes',prior.send(page,'hermes','不调用工具，只回复刚才的验证标记。'))
            check('Hermes inline strength switch affects only the next turn',first_h['reasoning_effort']=='low' and next_h['reasoning_effort']=='none' and marker_h in next_h['text'] and first_h['native_thread_id']==next_h['native_thread_id'])
            dsh=prior.new_private(page,'dsh-win','DSH 原生私聊')
            page.locator('#connect-dsh').click(); catalog=prior.wait_connection(page,'dsh')
            flash=next(model for model in catalog['models'] if json.loads(model['id'])[0]=='command-code-daily')
            pro=next(model for model in catalog['models'] if json.loads(model['id'])[0]=='command-code-daily-2')
            check('DSH catalog exposes both Hermes Command Code DS v4.1 routes',len(catalog['models'])==2 and all(json.loads(model['id'])[1]=='deepseek/deepseek-v4.1-flash' for model in catalog['models']) and all(value in flash['efforts'] for value in ('off','low','high','max')))
            settings(page,flash['id'],'off')
            marker_d=nonce()
            user=prior.send(page,'dsh',f'记住标记 {marker_d}。只回复这个标记，不调用工具。')
            prior.wait_run(page,'dsh',user,terminal=False)
            settings(page,pro['id'],'low')
            first_d=prior.wait_run(page,'dsh',user)
            check('DSH gives a real reply and freezes admitted settings',first_d['status']=='completed' and marker_d in first_d['text'] and first_d['model']==flash['id'] and first_d['reasoning_effort']=='off')
            second_d=prior.wait_run(page,'dsh',prior.send(page,'dsh','只回复刚才让我记住的标记，不解释。'))
            check('DSH switches model in the same native session',second_d['model']==pro['id'] and second_d['reasoning_effort']=='low' and second_d['native_thread_id']==first_d['native_thread_id'] and marker_d in second_d['text'])
            evidence.append(native_dsh_evidence(second_d))
            other=prior.new_private(page,'dsh-win','DSH 隔离私聊')
            settings(page,flash['id'],'off')
            isolated=prior.wait_run(page,'dsh',prior.send(page,'dsh','本会话此前是否给过你验证标记？如果没有，仅回复 ISOLATED。'))
            check('DSH private sessions keep separate context',isolated['native_thread_id']!=first_d['native_thread_id'] and 'ISOLATED' in isolated['text'] and marker_d not in isolated['text'])
            prior.select(page,dsh)
            page.locator('#message-input').fill('未发送的中文草稿')
            proc.terminate(); proc.wait(timeout=10); proc=None; time.sleep(.8)
            proc,endpoint=desktop.launch(); browser=playwright.chromium.connect_over_cdp(endpoint); page=desktop.page_for(browser)
            page.on('pageerror',lambda error:errors.append(str(error)))
            expect(page.locator('#message-input')).to_have_value('未发送的中文草稿')
            page.locator('#connect-dsh').click(); prior.wait_connection(page,'dsh')
            expect(page.locator('#live-model')).to_have_value(pro['id']); expect(page.locator('#live-effort')).to_have_value('low')
            resumed=prior.wait_run(page,'dsh',prior.send(page,'dsh','只回复本会话最初让我记住的验证标记。'))
            check('DSH restarts with saved settings, native session and context',resumed['native_thread_id']==first_d['native_thread_id'] and marker_d in resumed['text'] and resumed['model']==pro['id'] and resumed['reasoning_effort']=='low')
            page.locator('#live-reset').click(); page.wait_for_timeout(250)
            expect(page.locator('#live-model')).to_have_value(''); expect(page.locator('#live-effort')).to_have_value('')
            user=prior.send(page,'dsh','从1开始逐行列出所有整数，直到100000，不要停止，不解释。')
            prior.wait_run(page,'dsh',user,terminal=False)
            page.locator('#cancel-dsh').click(); stopped=prior.wait_run(page,'dsh',user)
            check('DSH stop waits for native cancellation',stopped['status']=='interrupted')
            recovered=prior.wait_run(page,'dsh',prior.send(page,'dsh','只回复 DSH_RECOVERED。'))
            check('DSH recovers after cancellation using default settings',recovered['status']=='completed' and 'DSH_RECOVERED' in recovered['text'] and recovered['model']==catalog['default_model'] and recovered['reasoning_effort']==catalog['default_effort'])
            page.screenshot(path=str(desktop.ARTIFACTS/'stage4-inline-dsh.png'))
            page.locator('#service-link').click(); page.locator('#service-dsh-control').click(); prior.wait_connection(page,'dsh','disconnected')
            check('DSH service control disconnects the owned harness')
            with sqlite3.connect(desktop.DATA/'hub.db') as db:
                check('schema persists three native agents',db.execute('PRAGMA user_version').fetchone()[0]>=4 and set(r[0] for r in db.execute('SELECT DISTINCT agent_id FROM runs'))=={'codex-win','hermes-win','dsh-win'})
            check('no JavaScript errors in real stage 4 flows',not errors)
            success=True
    finally:
        if proc and proc.poll() is None: proc.terminate(); proc.wait(timeout=10)
        (desktop.ARTIFACTS/'stage4-verification.json').write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks,'javascript_errors':errors,'native_codex_contexts':prior.native_contexts,'native_dsh_evidence':evidence,'test_data_directory':str(desktop.DATA),'input_method':'WebView2 CDP UI automation; no system input'},ensure_ascii=False,indent=2),encoding='utf-8')
    print('Stage 4: '+str(len(checks))+' passed',flush=True)

if __name__=='__main__':
    if '--evidence' not in sys.argv: run()
    validate_native_evidence()
