"""Read-only protocol handshake; never print auth, configuration, or raw stderr."""
import json
from pathlib import Path
import queue
import subprocess
import threading
import local_paths

exe = Path(local_paths.codex_executable())
if not exe.is_file():
    raise SystemExit('没找到 codex.exe：请用 AGENT_HUB_CODEX_EXE 指定绝对路径')
messages = queue.Queue()
process = subprocess.Popen([str(exe), 'app-server', '--listen', 'stdio://'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, encoding='utf-8', creationflags=subprocess.CREATE_NO_WINDOW)
threading.Thread(target=lambda: [messages.put(json.loads(line)) for line in process.stdout], daemon=True).start()

def rpc(identifier, method, params):
    process.stdin.write(json.dumps({'id': identifier, 'method': method, 'params': params}) + '\n')
    process.stdin.flush()
    while True:
        value = messages.get(timeout=30)
        if value.get('id') == identifier:
            if 'error' in value:
                raise RuntimeError(f'RPC failed: {method}, code={value["error"].get("code")}')
            return value['result']

try:
    rpc(1, 'initialize', {'clientInfo': {'name': 'personal_agent_hub', 'title': '同席 Agent Hub', 'version': '0.3.0'}})
    process.stdin.write('{"method":"initialized","params":{}}\n')
    process.stdin.flush()
    result = rpc(2, 'model/list', {})
    models = result.get('data', [])
    print(json.dumps({'handshake': 'ok', 'model_count': len(models), 'default_models': [m.get('model') for m in models if m.get('isDefault')], 'capabilities': [{'model': m.get('model'), 'efforts': m.get('supportedReasoningEfforts'), 'default_effort': m.get('defaultReasoningEffort')} for m in models]}, ensure_ascii=False))
finally:
    process.terminate()
    process.wait(timeout=10)
