"""Probe local Hermes ACP bridge; no model requests, no raw errors/auth output."""
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import uuid

root = Path(__file__).resolve().parents[1]
repo = Path(os.environ['LOCALAPPDATA']) / 'hermes/hermes-agent'
data = root / 'artifacts' / f'hermes-probe-{uuid.uuid4().hex[:10]}'
data.mkdir(parents=True)
messages = queue.Queue()
process = subprocess.Popen([str(repo / '.venv/Scripts/python.exe'), '-u', '-c', (root / 'src-tauri/src/hermes_bridge.py').read_text(encoding='utf-8'), str(repo), str(data)], env={**os.environ, 'HERMES_HOME': str(repo.parent)}, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, encoding='utf-8', creationflags=subprocess.CREATE_NO_WINDOW)

def reader():
    for line in process.stdout:
        try: messages.put(json.loads(line))
        except ValueError: pass
    messages.put({'eof': True})

threading.Thread(target=reader, daemon=True).start()

def rpc(identifier, method, params):
    process.stdin.write(json.dumps({'jsonrpc':'2.0','id':identifier,'method':method,'params':params}) + '\n')
    process.stdin.flush()
    while True:
        value = messages.get(timeout=90)
        if value.get('eof'): raise RuntimeError('Hermes native process exited')
        if value.get('id') == identifier:
            if 'error' in value: raise RuntimeError(f'{method} failed, code={value["error"].get("code")}')
            return value['result']

try:
    initialized = rpc(1, 'initialize', {'protocolVersion':1,'clientCapabilities':{'fs':{'readTextFile':False,'writeTextFile':False},'terminal':False},'clientInfo':{'name':'agent-hub-probe','version':'0.3.0'}})
    created = rpc(2, 'session/new', {'cwd':str(data),'mcpServers':[]})
    rpc(3, 'session/set_config_option', {'sessionId':created['sessionId'],'configId':'reasoning_effort','value':'low'})
    print(json.dumps({'handshake':'ok','version':initialized.get('agentInfo',{}).get('version'),'model_count':len(created.get('models',{}).get('availableModels',[])),'session_created':bool(created.get('sessionId')),'reasoning_setting_acknowledged':True}, ensure_ascii=False))
finally:
    process.terminate()
    process.wait(timeout=10)
