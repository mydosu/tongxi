"""Real native Codex dynamic-tool probe; writes only one owned fixture file."""
import json
import os
from pathlib import Path
import queue
import re
import shutil
import subprocess
import threading
import time
import uuid
from probe_project_tools import ToolSession, fixture

ROOT=Path(__file__).resolve().parents[1]
context=fixture();DATA=context['project'];helper=ToolSession(context)
exe=os.environ.get('AGENT_HUB_CODEX_EXE') or shutil.which('codex.exe')
if not exe:exe=r'D:\AI\codex\bin\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\codex.exe'
listed=subprocess.run([exe,'mcp','list','--json'],capture_output=True,text=True,encoding='utf-8',creationflags=subprocess.CREATE_NO_WINDOW,timeout=20)
if listed.returncode:raise RuntimeError('Cannot enumerate native MCP names safely')
names=[item['name'] for item in json.loads(listed.stdout)]
args=[exe,'app-server','--listen','stdio://','--disable','shell_tool','--disable','unified_exec']
for name in names:
    if not re.fullmatch(r'[A-Za-z0-9_-]+',name):raise RuntimeError('Unsupported MCP name for safe child override')
    args.extend(['-c','mcp_servers.'+name+'.enabled=false'])
process=subprocess.Popen(args,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True,encoding='utf-8',creationflags=subprocess.CREATE_NO_WINDOW)
messages=queue.Queue();threading.Thread(target=lambda:[messages.put(json.loads(line)) for line in process.stdout],daemon=True).start()
calls=[];unexpected=[];thread_id=None;success=False

def write(value):
    value['jsonrpc']='2.0'
    process.stdin.write(json.dumps(value,ensure_ascii=False)+'\n');process.stdin.flush()
def handle(value):
    if 'method' not in value:return
    if 'id' in value:
        if value['method']=='item/tool/call':
            params=value['params'];allowed=params['threadId']==thread_id and params['tool']=='hub_write'
            tool_args=params.get('arguments',{});allowed=allowed and tool_args.get('path')=='hello.txt' and tool_args.get('content')=='PROJECT_TOOL_OK'
            if allowed:
                tool_result=helper.call('hub_write',{**tool_args,'expected_sha256':None})
                allowed=not tool_result['isError']
                if allowed:calls.append(params['tool'])
            else:unexpected.append('rejected_dynamic_tool')
            write({'id':value['id'],'result':{'contentItems':[{'type':'inputText','text':'File saved' if allowed else 'Denied'}],'success':allowed}})
        else:
            unexpected.append(value['method']);write({'id':value['id'],'error':{'code':-32601,'message':'Unsupported request'}})
    elif value['method']=='item/started' and value['params'].get('item',{}).get('type') in ['commandExecution','mcpToolCall','fileChange']:
        unexpected.append(value['params']['item']['type'])

def rpc(identifier,method,params):
    write({'id':identifier,'method':method,'params':params});deadline=time.monotonic()+60
    while time.monotonic()<deadline:
        value=messages.get(timeout=60)
        if value.get('id')==identifier and 'method' not in value:
            if 'error' in value:raise RuntimeError('Native RPC failed: '+method+', code='+str(value['error'].get('code')))
            return value['result']
        handle(value)
    raise RuntimeError('Native RPC timeout')

try:
    rpc(1,'initialize',{'clientInfo':{'name':'agent_hub_project_probe','version':'0.7.0'},'capabilities':{'experimentalApi':True}});write({'method':'initialized','params':{}})
    models=rpc(2,'model/list',{})['data'];model=next((m for m in models if m.get('isDefault')),models[0])
    effort='low' if any(e['reasoningEffort']=='low' for e in model['supportedReasoningEfforts']) else model.get('defaultReasoningEffort')
    thread=rpc(3,'thread/start',{'cwd':str(DATA),'model':model['model'],'approvalPolicy':'never','sandbox':'read-only','ephemeral':True,'config':{'web_search':'disabled'},'developerInstructions':'仅使用 hub_write 工具写入此项目的 hello.txt。不要使用内置工具，不读取其他目录或凭据，不运行命令。','dynamicTools':[{'type':'function','name':'hub_write','description':'Write one authorized file in the bound project.','deferLoading':False,'inputSchema':{'type':'object','properties':{'path':{'type':'string'},'content':{'type':'string'}},'required':['path','content'],'additionalProperties':False}}]})
    thread_id=thread['thread']['id']
    turn=rpc(4,'turn/start',{'threadId':thread_id,'input':[{'type':'text','text':'请调用 hub_write，path 为 hello.txt，content 为 PROJECT_TOOL_OK，完成后简短回复。'}],'model':model['model'],'effort':effort,'approvalPolicy':'never','sandboxPolicy':{'type':'readOnly','networkAccess':False}})
    deadline=time.monotonic()+180
    while time.monotonic()<deadline:
        value=messages.get(timeout=60);handle(value)
        if value.get('method')=='turn/completed' and value['params']['turn']['id']==turn['turn']['id']:
            success=value['params']['turn']['status']=='completed' and calls==['hub_write'] and not unexpected and (DATA/'hello.txt').read_text(encoding='utf-8')=='PROJECT_TOOL_OK';break
    if not success:raise AssertionError('Dynamic project write was not verified')
finally:
    process.terminate();process.wait(timeout=10)
    helper.close()
    report={'success':success,'dynamic_tool_calls':calls,'unexpected_tool_count':len(unexpected),'disabled_mcp_count':len(names),'fixture':str(DATA),'broker':'real Rust stdio helper','native_policy':'readOnly; broker owns scoped writes; shell and unified exec disabled'}
    (ROOT/'artifacts/project-codex-probe.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf-8')
print(json.dumps(report,ensure_ascii=False))
