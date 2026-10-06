"""Real packaged project lifecycle and bounded repair, isolated owned fixtures.

Each scenario runs once and saves evidence even on failure. No native errors,
configuration, private prompts or credentials are printed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time
import uuid
from playwright.sync_api import sync_playwright
import smoke_desktop as desktop
import verify_stage3 as prior
import verify_stage6 as groups
from stage7_evidence import begin_executable_run, finish_executable_run

desktop.EXE=desktop.ROOT/'release/candidate/v0.7.0/Agent Hub.exe'
checks=[];errors=[];evidence=[]
REPORT=None

def check(name,condition=True):
    if not condition:raise AssertionError(name)
    checks.append(name);print('PASS '+name,flush=True)

def current(page,room):return desktop.ipc(page,'get_conversation',id=room)['workflows'][0]

def observe(page,room,predicate,timeout=360):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        job=current(page,room)
        if predicate(job):return job
        if job['status'] in ['failed','interrupted','completed']:
            raise AssertionError('Unexpected terminal project state: '+job['status'])
        page.wait_for_timeout(80)
    raise AssertionError('Project observation timed out; no automatic replay')

def terminal(page,room):return observe(page,room,lambda j:j['status'] in ['failed','interrupted','completed'])

def register(page,root,name,script):
    root.mkdir(parents=True)
    (root/'calc.py').write_text('def square(value):\n    return value\n',encoding='utf-8')
    (root/'check.py').write_text(script,encoding='utf-8')
    return desktop.ipc(page,'register_project',name=name,root=str(root),checks=[{'name':'固定功能检查','program':sys.executable,'args':['check.py'],'timeout_seconds':120}])

def room(page,title,project):
    value=groups.group(page,title,['hermes-win','codex-win','dsh-win'])
    desktop.ipc(page,'bind_project',conversationId=value,projectId=project['id'])
    desktop.ipc(page,'set_session_settings',id=value,agentId='hermes-win',model=None,reasoningEffort='none')
    desktop.ipc(page,'set_session_settings',id=value,agentId='codex-win',model=None,reasoningEffort='low')
    desktop.ipc(page,'set_session_settings',id=value,agentId='dsh-win',model=None,reasoningEffort='off')
    return value

def start(page,value,request):
    message=str(uuid.uuid4())
    return desktop.ipc(page,'start_project',conversationId=value,messageId=message,content=request)

def tree(root):
    # Snapshot only process identities; raw command lines are never requested.
    raw=subprocess.check_output(['powershell','-NoProfile','-Command','Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId | ConvertTo-Json -Compress'],creationflags=subprocess.CREATE_NO_WINDOW,text=True)
    rows=json.loads(raw);owned={root};changed=True
    while changed:
        added={r['ProcessId'] for r in rows if r['ParentProcessId'] in owned}-owned
        changed=bool(added);owned.update(added)
    return owned

def alive(ids):
    if not ids:return []
    expression='@('+','.join(map(str,sorted(ids)))+') | ForEach-Object { if (Get-Process -Id $_ -ErrorAction SilentlyContinue) { $_ } } | ConvertTo-Json -Compress'
    raw=subprocess.check_output(['powershell','-NoProfile','-Command',expression],creationflags=subprocess.CREATE_NO_WINDOW,text=True).strip()
    if not raw:return []
    value=json.loads(raw);return value if isinstance(value,list) else [value]

def wait_exit(ids):
    deadline=time.monotonic()+15
    while time.monotonic()<deadline:
        if not alive(ids):return True
        time.sleep(.3)
    return False

GOOD='from calc import square\nassert square(3)==9, "square(3) must be 9"\nassert square(-4)==16, "square(-4) must be 16"\nassert square(0)==0, "square(0) must be 0"\nprint("3 functional assertions passed")\n'
CORRECT='只分发一项任务，由 DSH 修复 calc.py 的 square(value)，返回 value 的平方，支持负数和零。只授权 calc.py，不能修改 check.py。固定检查通过后 Hermes 根据实际源码验收。'

def cancellation(page,root):
    project=register(page,root,'取消与重叠项目',GOOD)
    first=room(page,'原项目停止验收',project)
    child=register(page,root/'child','嵌套目录项目',GOOD)
    second=room(page,'嵌套项目排队验收',child)
    third=room(page,'超过协作上限验收',project)
    active=start(page,first,CORRECT)
    admitted=observe(page,first,lambda j:any(a['stage']=='plan' and a['agent_id']=='codex-win' and a['status']=='running' and a['native_turn_id'] for a in j['attempts']))
    queued=start(page,second,CORRECT);page.wait_for_timeout(250)
    pending=current(page,second)
    check('nested project waits for ancestor write lease without a native attempt',pending['status']=='queued' and not pending['attempts'])
    check('only two active workflows admitted',groups.reject(page,'start_project',conversationId=third,messageId=str(uuid.uuid4()),content=CORRECT))
    check('all project members protected from disconnect between phases',groups.reject(page,'disconnect_hermes') and groups.reject(page,'disconnect_codex'))
    desktop.ipc(page,'cancel_project',id=queued['id']);stopped_queue=terminal(page,second)
    check('queued cancellation sends no native model turn',stopped_queue['status']=='interrupted' and not stopped_queue['attempts'])
    check('queued unsent user request is marked failed',next(m for m in desktop.ipc(page,'get_conversation',id=second)['messages'] if m['id']==queued['user_message_id'])['status']=='failed')
    desktop.choose(page,first)
    button=page.locator('[data-stop-project="'+active['id']+'"]');button.click()
    stopped=terminal(page,first);page.wait_for_timeout(400)
    check('desktop stop terminates the initial Codex planner before dispatch',stopped['status']=='interrupted' and len(stopped['attempts'])==1 and stopped['attempts'][0]['stage']=='plan' and stopped['attempts'][0]['agent_id']=='codex-win' and stopped['attempts'][0]['status']=='interrupted' and not stopped['tasks'])
    check('stopped workflows release every lease and active job',not desktop.ipc(page,'project_status'))
    check('cancelled planner cannot modify project files',(root/'calc.py').read_text(encoding='utf-8')=='def square(value):\n    return value\n')
    check('technical services remain connected after project stop',desktop.ipc(page,'hermes_status')['connection']=='connected' and desktop.ipc(page,'codex_status')['connection']=='connected')
    evidence.extend([stopped,stopped_queue])

def crash(page,root,proc,playwright):
    project=register(page,root,'崩溃恢复项目',GOOD)
    value=room(page,'实现中崩溃验收',project)
    request='只由 DSH 实现 calc.py，写 square(value) 返回平方，并写同文件内的 describe_square(value)，生成一段不少于3000字的中文数学说明字符串，说明负数、零、整数与浮点数的平方及50个编号示例。只授权 calc.py，不能修改 check.py。框架随后实际验收。'
    start(page,value,request)
    running=observe(page,value,lambda j:any(a['stage']=='implement' and a['agent_id']=='dsh-win' and a['status']=='running' and a['native_turn_id'] for a in j['attempts']))
    children=tree(proc.pid)-{proc.pid};check('real native implementer admitted before crash',bool(children))
    plan_attempts=[a for a in running['attempts'] if a['stage']=='plan']
    check('Codex proposal and Hermes dispatch both complete before implementation crash',[a['agent_id'] for a in plan_attempts]==['codex-win','hermes-win'] and all(a['status']=='completed' for a in plan_attempts))
    proc.terminate();proc.wait(timeout=10)
    check('hard desktop exit cleans all observed owned Windows children',wait_exit(children))
    proc,endpoint=desktop.launch();browser=playwright.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser)
    restored=current(page,value);page.wait_for_timeout(450)
    check('crashed project restored as interrupted without automatic replay',restored['id']==running['id'] and restored['status']=='interrupted' and len(restored['attempts'])==len(running['attempts']) and not desktop.ipc(page,'project_status'))
    check('crashed native attempt and task restore as interrupted',any(a['stage']=='implement' and a['agent_id']=='dsh-win' and a['status']=='interrupted' for a in restored['attempts']) and restored['tasks'][0]['status']=='interrupted')
    with sqlite3.connect(desktop.DATA/'hub.db') as db:
        check('restart releases write leases and retains referential integrity',db.execute('SELECT COUNT(*) FROM project_leases').fetchone()[0]==0 and not db.execute('PRAGMA foreign_key_check').fetchall())
    evidence.append(restored)
    return proc,page

def repair(page,root,permanent=False):
    script=('print("External fixed acceptance condition unavailable; cannot fix through source files")\nraise SystemExit(1)\n' if permanent else GOOD)
    project=register(page,root,'永久失败上限' if permanent else '真实失败修复',script)
    before=hashlib.md5((root/'check.py').read_bytes()).hexdigest()
    value=room(page,'一次复杂修复验收',project)
    request=('只分发一项由 DSH 执行的任务：calc.py 的 square(value) 返回平方，支持负数零；只授权 calc.py。check.py 是外部固定验收，不允许修改；若外部条件失败，最多修复一次并诚实记录失败。' if permanent else '这是隔离的框架修复流程验收，只授权 calc.py。最终产品要求 square(value) 返回平方，支持负数和零。为了真实验证失败修复链路，初始实现任务故意要求 DSH 把 square(value) 写成 return value，不要在初始任务内先纠正它。Hermes 必须只分发这一项初始任务，禁止修改 check.py。随后由框架运行固定 check.py，失败后框架再调用 DSH 的一次复杂修复，将 square 改为真正的平方。最后 Hermes 按最终产品要求及实际检查结果验收。')
    start(page,value,request);done=terminal(page,value)
    verifies=[a for a in done['attempts'] if a['stage']=='verify'];repairs=[a for a in done['attempts'] if a['stage']=='repair']
    check('one Codex diagnosis and one DSH repair with two checks',len(verifies)==2 and verifies[0]['status']=='failed' and verifies[0]['checks'][0]['exit_code']==1 and len(repairs)==2 and [a['agent_id'] for a in repairs]==['codex-win','dsh-win'] and all(a['status']=='completed' for a in repairs) and all(k in json.loads(repairs[0]['output']) for k in ['summary','instructions']))
    if permanent:
        check('permanent external failure ends honestly without unbounded retry',done['status']=='failed' and verifies[1]['status']=='failed' and not any(a['stage']=='review' for a in done['attempts']))
    else:
        check('one complex repair passes three real functional assertions and Hermes review',done['status']=='completed' and verifies[1]['checks'][0]['exit_code']==0 and '3 functional assertions passed' in verifies[1]['checks'][0]['output'] and done['attempts'][-1]['stage']=='review')
        dsh_repair_attempts={a.get('id') for a in repairs if a['stage']=='repair' and a['agent_id']=='dsh-win'}
        writers={a.get('id'):a['agent_id'] for a in done['attempts']}
        def change_writer(change):return change.get('agent_id') or writers.get(change.get('attempt_id'))
        check('repair evidence records an actual calc.py write by the DSH repair attempt',any(c['path']=='calc.py' and c.get('attempt_id') in dsh_repair_attempts and change_writer(c)=='dsh-win' for c in done['changes']))
        check('every recorded change is written by DSH and the Codex diagnosis records none',bool(done['changes']) and all(change_writer(c)=='dsh-win' for c in done['changes']))
    check('immutable acceptance script survives all repair steps',hashlib.md5((root/'check.py').read_bytes()).hexdigest()==before)
    check('failed or repaired terminal project releases write lease',not desktop.ipc(page,'project_status'))
    evidence.append(done)
    plan_attempts=[a for a in done['attempts'] if a['stage']=='plan']
    check('workflow records Codex proposal before Hermes dispatch',[a['agent_id'] for a in plan_attempts]==['codex-win','hermes-win'] and all(a['status']=='completed' for a in plan_attempts))

def run(scenario):
    global REPORT
    desktop.DATA=desktop.ARTIFACTS/('stage7-'+scenario+'-'+uuid.uuid4().hex[:10])
    root=desktop.ARTIFACTS/('stage7-'+scenario+'-fixture-'+uuid.uuid4().hex[:10])
    REPORT=desktop.ARTIFACTS/('stage7-'+scenario+'-verification.json')
    proc=None;success=False;executable_provenance=begin_executable_run(desktop.EXE)
    try:
        with sync_playwright() as playwright:
            proc,endpoint=desktop.launch();browser=playwright.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser)
            page.on('pageerror',lambda error:errors.append(str(error)))
            desktop.ipc(page,'connect_hermes');desktop.ipc(page,'connect_codex');desktop.ipc(page,'connect_dsh')
            prior.wait_connection(page,'hermes');prior.wait_connection(page,'codex');prior.wait_connection(page,'dsh')
            if scenario=='cancel':cancellation(page,root)
            elif scenario=='crash':proc,page=crash(page,root,proc,playwright)
            else:repair(page,root,scenario=='permanent')
            check('no JavaScript runtime errors',not errors);success=True
    finally:
        if proc and proc.poll() is None:proc.terminate();proc.wait(timeout=10)
        REPORT.write_text(json.dumps({'scenario':scenario,'success':success,'passed':len(checks),'checks':checks,'javascript_errors':errors,'test_data_directory':str(desktop.DATA),'project_root':str(root),'evidence':evidence,**finish_executable_run(desktop.EXE,executable_provenance)},ensure_ascii=False,indent=2),encoding='utf-8')

if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('scenario',choices=['cancel','crash','repair','permanent']);args=parser.parse_args()
    run(args.scenario);print('Stage7 '+args.scenario+': '+str(len(checks))+' passed',flush=True)
