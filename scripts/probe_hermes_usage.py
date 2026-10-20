"""Hermes 回合用量回报：跑一次极小的真实对话（只回一个字），确认 session/prompt 的回包带 usage。
会产生一次很小的模型调用；用隔离的会话库，不动 Hermes 的真实会话。"""
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import uuid

root = Path(__file__).resolve().parents[1]
repo = Path(os.environ['LOCALAPPDATA']) / 'hermes/hermes-agent'
data = Path(os.environ['TEMP']) / f'hermes-usage-probe-{uuid.uuid4().hex[:10]}'
data.mkdir(parents=True)
messages = queue.Queue()
env = {**os.environ, 'HERMES_HOME': str(repo.parent), 'AGENT_HUB_HERMES_SESSION_DB': str(data / 'sessions.db')}
process = subprocess.Popen([str(repo / '.venv/Scripts/python.exe'), '-u', '-c', (root / 'src-tauri/src/hermes_bridge.py').read_text(encoding='utf-8'), str(repo), str(data)], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, encoding='utf-8', creationflags=subprocess.CREATE_NO_WINDOW)


def reader():
    for line in process.stdout:
        try:
            messages.put(json.loads(line))
        except ValueError:
            pass
    messages.put({'eof': True})


threading.Thread(target=reader, daemon=True).start()


def rpc(identifier, method, params, timeout=180):
    process.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': identifier, 'method': method, 'params': params}) + '\n')
    process.stdin.flush()
    while True:
        value = messages.get(timeout=timeout)
        if value.get('eof'):
            raise RuntimeError('Hermes native process exited')
        if value.get('id') == identifier:
            if 'error' in value:
                raise RuntimeError(f'{method} failed, code={value["error"].get("code")}')
            return value['result']


try:
    rpc(1, 'initialize', {'protocolVersion': 1, 'clientCapabilities': {'fs': {'readTextFile': False, 'writeTextFile': False}, 'terminal': False}, 'clientInfo': {'name': 'agent-hub-probe', 'version': '2.0.0'}})
    created = rpc(2, 'session/new', {'cwd': str(data), 'mcpServers': []})
    rpc(3, 'session/set_config_option', {'sessionId': created['sessionId'], 'configId': 'reasoning_effort', 'value': 'low'})
    result = rpc(4, 'session/prompt', {'sessionId': created['sessionId'], 'prompt': [{'type': 'text', 'text': '只回复一个字：好'}]})
    print(json.dumps({'stopReason': result.get('stopReason'), 'usage': result.get('usage')}, ensure_ascii=False))
finally:
    process.terminate()
