"""Two real DS v4.1 routes write only isolated project files through scoped MCP."""
import json
import os
from pathlib import Path
import queue
import shutil
import sqlite3
import subprocess
import threading
import time
import sys
from probe_project_tools import ROOT, EXE, fixture

INSTALLATION=Path(os.environ.get('AGENT_HUB_DSH_INSTALLATION',r'D:\AI\dsh\bin\node_modules\@deepseek-ai\dsh'))
NODE=os.environ.get('AGENT_HUB_DSH_NODE') or shutil.which('node.exe')
if not NODE:raise RuntimeError('Native Node executable unavailable')

def run(index):
    context=fixture('dsh-win');native=context['data']/'dsh-project-native';native.mkdir()
    env=os.environ.copy();env['AGENT_HUB_DSH_PROJECT_MODE']='1'
    env['AGENT_HUB_DSH_AUTH_BRIDGE']=(ROOT/'src-tauri/src/dsh_hermes_auth.py').read_text(encoding='utf-8')
    source=(ROOT/'src-tauri/src/dsh_bridge.mjs').read_text(encoding='utf-8')
    proc=subprocess.Popen([NODE,'--input-type=module','-e',source,str(INSTALLATION),str(native)],env=env,cwd=context['data'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True,encoding='utf-8',creationflags=subprocess.CREATE_NO_WINDOW)
    messages=queue.Queue();updates=[];unexpected=[];identifier=0
    def read():
        for line in proc.stdout:
            try:messages.put(json.loads(line))
            except json.JSONDecodeError:messages.put({'invalid_protocol':True})
    threading.Thread(target=read,daemon=True).start()
    def write(value):value['jsonrpc']='2.0';proc.stdin.write(json.dumps(value,ensure_ascii=False)+'\n');proc.stdin.flush()
    def handle(value):
        if value.get('method')=='session/update':updates.append(value.get('params',{}).get('update',{}))
        elif 'id' in value and 'method' in value:
            unexpected.append(value['method']);write({'id':value['id'],'error':{'code':-32601,'message':'Only scoped project MCP tools are allowed'}})
        elif value.get('invalid_protocol'):raise RuntimeError('Native ACP emitted invalid protocol')
    def rpc(method,params,timeout=60):
        nonlocal identifier
        identifier+=1;current=identifier;write({'id':current,'method':method,'params':params});deadline=time.monotonic()+timeout
        while time.monotonic()<deadline:
            try:value=messages.get(timeout=min(10,max(.1,deadline-time.monotonic())))
            except queue.Empty:
                if proc.poll() is not None:raise RuntimeError('Native DSH disconnected')
                continue
            if value.get('id')==current and 'method' not in value:
                if 'error' in value:raise RuntimeError('Native ACP failed: '+method+', code='+str(value['error'].get('code')))
                return value['result']
            handle(value)
        raise RuntimeError('Native ACP timeout: '+method)
    try:
        rpc('initialize',{'protocolVersion':1,'clientCapabilities':{'fs':{'readTextFile':False,'writeTextFile':False},'terminal':False},'clientInfo':{'name':'agent_hub_project_probe','version':'0.7.0'}})
        session=rpc('session/new',{'cwd':str(context['project']),'mcpServers':[{'name':'agent_hub','command':str(EXE),'args':['--project-tools',str(context['db']),context['attempt']],'env':[]}]})
        sid=session['sessionId'];provider='command-code-daily'+('-2' if index==2 else '')
        model=json.dumps([provider,'deepseek/deepseek-v4.1-flash'],separators=(',',':'))
        rpc('session/set_config_option',{'sessionId':sid,'configId':'model','value':model})
        rpc('session/set_config_option',{'sessionId':sid,'configId':'reasoning_effort','value':'off'})
        result=rpc('session/prompt',{'sessionId':sid,'prompt':[{'type':'text','text':'这是隔离项目编码验收。请使用 mcp__agent_hub__hub_write 在当前任务范围内创建 calc.py，内容严格为 def add(a, b):\n    return a + b\n。expected_sha256 使用 null，因为文件尚不存在。只需要写这一份文件，然后简短说明已经写入；不要运行命令，验收由框架负责。'}]},180)
        if result.get('stopReason')!='end_turn' or unexpected:raise AssertionError('Native DSH task did not complete cleanly')
        file=context['project']/'calc.py'
        if not file.is_file():raise AssertionError('Native DSH did not write the project file')
        checks=subprocess.run([os.sys.executable,'-c','from calc import add; assert add(2,3)==5; assert add(5,-7)==-2; assert add(0,0)==0; print("3 checks passed")'],cwd=context['project'],capture_output=True,text=True,timeout=10,creationflags=subprocess.CREATE_NO_WINDOW)
        if checks.returncode:raise AssertionError('Native generated function failed actual verification')
        with sqlite3.connect(context['db']) as db:
            count=db.execute('SELECT count(*) FROM project_changes WHERE attempt_id=?',(context['attempt'],)).fetchone()[0]
        if count!=1:raise AssertionError('Native write journal missing or unexpected changes')
        tools=[u for u in updates if u.get('sessionUpdate') in ['tool_call','tool_call_update']]
        if not tools:raise AssertionError('Native MCP tool events were not observed')
        return {'provider':provider,'model':'deepseek/deepseek-v4.1-flash','reasoning_effort':'off','native_session_id':sid,'file_changes':count,'functional_checks':3,'tool_events':len(tools),'unexpected_client_requests':len(unexpected),'fixture':str(context['base'])}
    finally:
        if proc.poll() is None:
            proc.stdin.close()
            try:proc.wait(timeout=10)
            except subprocess.TimeoutExpired:proc.terminate();proc.wait(timeout=5)

def validate_evidence(route):
    expected={'mcp__agent_hub__hub_'+name for name in ['list','read','write','edit','delete']}
    contexts=[]
    for path in (Path(route['fixture'])/'data/dsh-project-native/sessions').rglob('*.jsonl'):
        for line in path.read_text(encoding='utf-8').splitlines():
            event=json.loads(line)
            if event.get('type')!='request/header':continue
            header=event.get('data',{}).get('header',{});config=header.get('config',{})
            context={key:config.get(key) for key in ['provider','model','reasoningEffort']}
            if context!={'provider':route['provider'],'model':route['model'],'reasoningEffort':'off'}:raise AssertionError('Native DSH request configuration differs from selected settings')
            names={tool.get('name') for tool in header.get('tools',[])}
            if names!=expected:raise AssertionError('Native DSH tool list differs from the scoped broker')
            contexts.append({**context,'tool_count':len(names)})
    if not contexts:raise AssertionError('Native DSH request evidence missing')
    route['native_request_evidence']=contexts
    return route

if __name__=='__main__' and '--evidence-only' in sys.argv:
    path=ROOT/'artifacts/project-dsh-probe.json';report=json.loads(path.read_text(encoding='utf-8'))
    for route in report['routes']:validate_evidence(route)
    path.write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf-8')
    print(json.dumps({'native_evidence_routes':len(report['routes']),'model_requests':0}))
elif __name__=='__main__':
    reports=[]
    for index in [1,2]:
        reports.append(validate_evidence(run(index)))
        (ROOT/'artifacts/project-dsh-probe.json').write_text(json.dumps({'routes':reports,'passed_routes':len(reports)},ensure_ascii=False,indent=2),encoding='utf-8')
        print(json.dumps({'passed_routes':len(reports),'functional_checks':sum(r['functional_checks'] for r in reports)}),flush=True)
