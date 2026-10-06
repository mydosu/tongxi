"""Packaged project workflow, real native models, immutable checks, isolated data."""
import hashlib
import json
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid
from playwright.sync_api import sync_playwright, expect
import smoke_desktop as desktop
import verify_stage3 as prior
import verify_stage6 as groups
from stage7_evidence import begin_executable_run, finish_executable_run

desktop.EXE=desktop.ROOT/'release/candidate/v0.7.0/Agent Hub.exe'
desktop.DATA=desktop.ARTIFACTS/('stage7-desktop-'+uuid.uuid4().hex[:10])
checks=[];errors=[];evidence=[]

def check(name,condition=True):
    if not condition:raise AssertionError(name)
    checks.append(name);print('PASS '+name,flush=True)

def migration():
    report=json.loads((desktop.ARTIFACTS/'stage6-verification.json').read_text(encoding='utf-8'))
    source=Path(report['test_data_directory'])/'hub.db';desktop.DATA.mkdir(parents=True)
    with sqlite3.connect('file:'+source.as_posix()+'?mode=ro',uri=True) as old,sqlite3.connect(desktop.DATA/'hub.db') as new:
        if old.execute('PRAGMA user_version').fetchone()[0]!=6:raise AssertionError('Actual historical v6 fixture unavailable')
        old.backup(new)
        return {table:([row[1] for row in old.execute('PRAGMA table_info('+table+')')],old.execute('SELECT * FROM '+table+' ORDER BY rowid').fetchall()) for table in ['conversations','members','sessions','messages','runs','discussions']}

def workflow(page,room):return desktop.ipc(page,'get_conversation',id=room)['workflows'][0]
def wait(page,room,predicate,timeout=420):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        job=workflow(page,room)
        if predicate(job):return job
        if job['status'] in ['failed','interrupted']:
            raise AssertionError('Project workflow terminated: '+job['status']+' / '+str(job.get('error')))
        page.wait_for_timeout(150)
    raise AssertionError('Project workflow observation timeout')

def dsh_routes(catalog):
    routes=[model for model in catalog['models'] if json.loads(model['id'])[0] in ['command-code-daily','command-code-daily-2']]
    if len(routes)<2:raise AssertionError('Two serial DSH daily routes unavailable')
    return routes

def native_evidence(job):
    result=[]
    for attempt in job['attempts']:
        folder=desktop.DATA/'project-native'/attempt['id']
        if attempt['agent_id']=='hermes-win' and attempt['native_thread_id']:
            source=folder/'hermes-native/sessions.db';parts=[source]+[Path(str(source)+suffix) for suffix in ['-wal','-shm'] if Path(str(source)+suffix).exists()];digest=lambda:hashlib.md5(b''.join(part.read_bytes() for part in parts)).hexdigest();before=digest();prompts=[]
            with tempfile.TemporaryDirectory() as temporary:
                copy=Path(temporary)/source.name;shutil.copyfile(source,copy)
                for suffix in ['-wal','-shm']:
                    sidecar=Path(str(source)+suffix)
                    if sidecar.exists():shutil.copyfile(sidecar,Path(str(copy)+suffix))
                with sqlite3.connect(copy) as db:
                    prompts=[json.loads(row[0]) for row in db.execute("SELECT content FROM messages WHERE session_id=? AND role='user' ORDER BY id",[attempt['native_thread_id']])]
                db.close()
            if digest()!=before:raise AssertionError('Read-only Hermes native session source changed while collecting evidence')
            result.append({'stage':attempt['stage'],'agent':attempt['agent_id'],'prompt_count':len(prompts),'has_current_request':all(prompt.get('request')==job['request'] for prompt in prompts),'has_codex_plan':attempt['stage']=='plan' and all(isinstance(prompt.get('codex_plan'),dict) for prompt in prompts),'prompts':prompts})
        if attempt['agent_id']=='dsh-win':
            headers=[]
            for path in (folder/'dsh-native/sessions').rglob('*.jsonl'):
                for line in path.read_text(encoding='utf-8').splitlines():
                    event=json.loads(line)
                    if event.get('type')=='request/header':
                        header=event.get('data',{}).get('header',{});config=header.get('config',{})
                        headers.append({'config':{k:config.get(k) for k in ['provider','model','reasoningEffort']},'tools':[tool.get('name') for tool in header.get('tools',[])]})
            result.append({'stage':attempt['stage'],'agent':attempt['agent_id'],'task_id':attempt.get('task_id'),'headers':headers})
    return result

def dsh_headers(native,job):
    lookup={item['task_id']:item['headers'] for item in native if item['agent']=='dsh-win' and item['task_id'] is not None}
    return [lookup.get(task.get('id',task.get('task_id'))) or [] for task in job['tasks']]

def run():
    proc=None;success=False;old=migration();fixture=desktop.ARTIFACTS/('stage7-fixture-'+uuid.uuid4().hex[:10]);fixture.mkdir();executable_provenance=begin_executable_run(desktop.EXE)
    (fixture/'arithmetic.py').write_text('def multiply(a, b):\n    return a + b\n',encoding='utf-8')
    (fixture/'labels.py').write_text('def format_total(value):\n    return "wrong:" + str(value)\n',encoding='utf-8')
    checker=fixture/'check.py';checker.write_text('from arithmetic import multiply\nfrom labels import format_total\nassert multiply(3, 4) == 12\nassert multiply(-2, 5) == -10\nassert multiply(0, 9) == 0\nassert format_total(12) == "total:12"\nassert format_total(-1) == "total:-1"\nassert format_total(0) == "total:0"\nprint("6 actual functional assertions passed")\n',encoding='utf-8')
    before=hashlib.md5(checker.read_bytes()).hexdigest()
    try:
        with sync_playwright() as playwright:
            proc,endpoint=desktop.launch();browser=playwright.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser)
            page.on('pageerror',lambda error:errors.append(str(error)))
            with sqlite3.connect(desktop.DATA/'hub.db') as db:
                check('actual v6 migration preserves every old record and mapping',db.execute('PRAGMA user_version').fetchone()[0]==7 and not db.execute('PRAGMA foreign_key_check').fetchall() and all(db.execute('SELECT '+','.join(cols)+' FROM '+table+' ORDER BY rowid').fetchall()==rows for table,(cols,rows) in old.items()))
            backups=list((desktop.DATA/'backups').glob('hub-schema-v6-*.db'));check('one complete pre-migration backup',len(backups)==1)
            with sqlite3.connect('file:'+backups[0].as_posix()+'?mode=ro',uri=True) as db:
                check('backup retains readable original schema and all records',db.execute('PRAGMA user_version').fetchone()[0]==6 and all(db.execute('SELECT * FROM '+table+' ORDER BY rowid').fetchall()==rows for table,(_,rows) in old.items()))
            room=groups.group(page,'项目协作隔离验收',['hermes-win','codex-win','dsh-win'])
            check('project binding control visible',page.locator('#bind-project').is_visible())
            page.locator('#bind-project').click();page.locator('#project-name').fill('隔离功能项目');page.locator('#project-root').fill(str(desktop.DATA));page.locator('#check-program').fill(sys.executable);page.locator('#check-args').fill('["check.py"]');page.locator('#project-form [type="submit"]').click()
            expect(page.locator('#project-form .form-error')).to_contain_text('不能与同席数据目录重叠');check('desktop binding rejects app data as a writable project')
            page.locator('#project-root').fill(str(fixture));page.locator('#project-form [type="submit"]').click()
            expect(page.locator('#project-title')).to_contain_text('隔离功能项目');check('project registered and bound through desktop UI',desktop.ipc(page,'get_conversation',id=room)['project']['checks'][0]['args']==['check.py'])
            page.locator('#connect-discussion').click()
            catalogs={key:prior.wait_connection(page,key) for key in ['hermes','codex','dsh']}
            check('all technical members connect using existing native harnesses',all(catalog['models'] for catalog in catalogs.values()))
            model=catalogs['codex']['default_model'];first,second=dsh_routes(catalogs['dsh'])
            groups.live_settings(page,'codex-win',model,'low');groups.live_settings(page,'hermes-win',None,'low');groups.live_settings(page,'dsh-win',first['id'],'off')
            draft='UNSENT_PROJECT_DRAFT_'+uuid.uuid4().hex[:8]
            desktop.ipc(page,'save_local_message',conversationId=room,messageId=str(uuid.uuid4()),content=draft)
            request='修复这个隔离项目的两个函数，严格按两项任务串行分发。第一项由 DSH 负责 arithmetic.py：multiply(a,b) 必须返回乘积，支持负数和零。第二项依赖第一项，由 DSH 负责 labels.py：format_total(value) 必须返回 "total:" 加参数的字符串形式。只修改这两个文件，不新增文件。check.py 是我提前准备的固定验收脚本，不允许修改。框架运行 check.py 后，由 Hermes 根据源码和真实结果进行功能校验。'
            page.locator('#message-input').fill(request);expect(page.locator('#send-project')).to_be_enabled(timeout=15000);page.locator('#send-project').click()
            admitted=wait(page,room,lambda job:any(a['agent_id']=='dsh-win' and a['stage']=='implement' and a['status']=='running' for a in job['attempts']))
            first_implement=next(a for a in admitted['attempts'] if a['agent_id']=='dsh-win' and a['stage']=='implement')
            check('Hermes returns two serial DSH implement assignments',len(admitted['tasks'])==2 and [task['agent_id'] for task in admitted['tasks']]==['dsh-win','dsh-win'] and admitted['tasks'][1]['depends_on']==[0])
            check('active project blocks discussion and rebind UI',page.locator('#send-discussion').is_disabled() and page.locator('#bind-project').is_disabled())
            check('active agent cannot be disconnected or reused for private work',groups.reject(page,'disconnect_dsh'))
            groups.live_settings(page,'dsh-win',second['id'],'low');groups.live_settings(page,'hermes-win',None,'none')
            current=workflow(page,room);locked=next(a for a in current['attempts'] if a['id']==first_implement['id']);check('current native DSH attempt freezes first route and effort off',locked['model']==first['id'] and locked['reasoning_effort']=='off')
            done=wait(page,room,lambda job:job['status']=='completed')
            check('real end-to-end project workflow completes',done['status']=='completed' and all(task['status']=='completed' for task in done['tasks']))
            plan_agents=[attempt['agent_id'] for attempt in done['attempts'] if attempt['stage']=='plan']
            check('Codex formulates the project plan before Hermes dispatches it',plan_agents==['codex-win','hermes-win'] and all(attempt['status']=='completed' for attempt in done['attempts'] if attempt['stage']=='plan'))
            check('both implementers changed actual authorized files',{change['path'] for change in done['changes']}=={'arithmetic.py','labels.py'})
            verification=next(a for a in reversed(done['attempts']) if a['stage']=='verify');check('actual configured check produces exit zero and six functional assertions',verification['status']=='completed' and len(verification['checks'])==1 and verification['checks'][0]['exit_code']==0 and '6 actual functional assertions passed' in verification['checks'][0]['output'])
            review=next(a for a in reversed(done['attempts']) if a['stage']=='review');check('Hermes approves structured review with next-phase live effort',review['reasoning_effort']=='none' and json.loads(review['output'].strip().removeprefix('```json\n').removesuffix('```').strip())['approved'])
            check('fixed check script unchanged',hashlib.md5(checker.read_bytes()).hexdigest()==before)
            check('real changes have before-images',len(list((desktop.DATA/'project-backups').rglob('*.bin')))>=2)
            details=desktop.ipc(page,'get_conversation',id=room);check('no internal fake user or agent chat messages',len(details['messages'])==2 and all(message['sender_id']=='user' for message in details['messages']))
            native=native_evidence(done);headers=dsh_headers(native,done);dispatch=next(item for item in native if item['stage']=='plan' and item['agent']=='hermes-win');reviewing=next(item for item in native if item['stage']=='review')
            check('Hermes dispatch receives the Codex plan and current request without unsent draft',dispatch['prompt_count']>0 and dispatch['has_current_request'] and dispatch['has_codex_plan'] and draft not in json.dumps(dispatch['prompts'],ensure_ascii=False))
            check('native Hermes review receives real source and check results',reviewing['has_current_request'] and all(prompt.get('sources') and prompt.get('actual_checks') for prompt in reviewing['prompts']))
            allowed={'mcp__agent_hub__hub_'+name for name in ['list','read','write','edit','delete']}
            check('native DSH first task uses initial daily route with effort off and scoped tools',bool(headers[0]) and all(header['config']=={'provider':'command-code-daily','model':json.loads(first['id'])[1],'reasoningEffort':'off'} and set(header['tools'])==allowed for header in headers[0]))
            check('native DSH second task uses switched DS4.1 route and low with scoped tools',bool(headers[1]) and all(header['config']=={'provider':'command-code-daily-2','model':'deepseek/deepseek-v4.1-flash','reasoningEffort':'low'} and set(header['tools'])==allowed for header in headers[1]))
            expect(page.locator('[data-workflow-id="'+done['id']+'"] .workflow-status')).to_have_text('协作完成');check('desktop task board shows final native result')
            page.screenshot(path=str(desktop.ARTIFACTS/'stage7-project-completed.png'))
            evidence.append({'workflow':done,'native_metadata':[{k:v for k,v in item.items() if k!='prompts'} for item in native]})
            check('completed workflow releases every project lease',not desktop.ipc(page,'project_status'))
            proc.terminate();proc.wait(timeout=10);proc=None;time.sleep(.4)
            proc,endpoint=desktop.launch();browser=playwright.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser)
            restored=desktop.ipc(page,'get_conversation',id=room);check('actual restart restores project task results and changes',restored['project']['id']==details['project']['id'] and restored['workflows'][0]['status']=='completed' and restored['workflows'][0]['changes']==done['changes'])
            check('restart does not replay completed native project work',len(restored['workflows'][0]['attempts'])==len(done['attempts']) and not desktop.ipc(page,'project_status'))
            check('no JavaScript runtime errors',not errors);success=True
    finally:
        if proc is not None and proc.poll() is None:proc.terminate();proc.wait(timeout=10)
        report={'success':success,'passed':len(checks),'checks':checks,'javascript_errors':errors,'test_data_directory':str(desktop.DATA),'evidence':evidence,**finish_executable_run(desktop.EXE,executable_provenance)}
        (desktop.ARTIFACTS/'stage7-verification.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf-8')

def resume_finished():
    path=desktop.ARTIFACTS/'stage7-verification.json';report=json.loads(path.read_text(encoding='utf-8'))
    if not report['evidence'] or report['evidence'][0]['workflow']['status']!='completed':raise AssertionError('No authoritative completed workflow to resume checking')
    desktop.DATA=Path(report['test_data_directory']);checks.extend(report['checks']);errors.extend(report['javascript_errors']);done=report['evidence'][0]['workflow'];proc=None
    try:
        with sync_playwright() as playwright:
            proc,endpoint=desktop.launch();browser=playwright.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser)
            page.on('pageerror',lambda error:errors.append(str(error)))
            restored=desktop.ipc(page,'get_conversation',id=done['conversation_id'])
            check('actual restart restores project task results and changes',restored['project']['id']==done['project_id'] and restored['workflows'][0]['status']=='completed' and restored['workflows'][0]['changes']==done['changes'])
            check('restart does not replay completed native project work',len(restored['workflows'][0]['attempts'])==len(done['attempts']) and not desktop.ipc(page,'project_status'))
            expect(page.locator('[data-workflow-id="'+done['id']+'"] .workflow-status')).to_have_text('协作完成');check('restart restores the completed task board')
            check('no JavaScript runtime errors',not errors)
            report.update({'success':True,'passed':len(checks),'checks':checks,'javascript_errors':errors,'restart_check_resumed':True})
            path.write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf-8')
    finally:
        if proc and proc.poll() is None:proc.terminate();proc.wait(timeout=10)

if __name__=='__main__':
    if '--resume-finished' in sys.argv:resume_finished()
    else:run()
    print('Stage7 project workflow: '+str(len(checks))+' passed',flush=True)
