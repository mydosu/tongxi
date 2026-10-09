"""Diagnose a native DSH tool turn without printing upstream errors or credentials."""
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import sqlite3
import sys
import tempfile
import threading
import time

import local_paths

ROOT = Path(__file__).resolve().parents[1]
KNOWN_TERMS = ['429', '402', '401', '403', '404', 'quota', 'rate limit', 'rate_limit',
               'insufficient', 'not found', 'not registered', 'undefined', 'not a function',
               'schema', 'reasoning_content', 'max_tokens', 'context length', 'connection',
               'fetch failed', 'sandbox', 'sessionprojection', 'sessionquery', 'tools']


def safe_error(error):
    raw = json.dumps(error).lower()
    return {'code': error.get('code'), 'known_terms': [term for term in KNOWN_TERMS if term in raw],
            'message_characters': len(str(error.get('message', ''))),
            'modules': re.findall(r'@deepseek-ai/[a-z0-9-]+', raw)}


def run():
    root = Path(tempfile.mkdtemp(prefix='tongxi-dsh-diagnostic-'))
    (root / 'calc.py').write_text('def square(value):\n    return value\n', encoding='utf-8')
    servers = []
    if '--mcp' in sys.argv:
        report = json.loads((ROOT / 'artifacts/project-roles-ui-verification.json').read_text(encoding='utf-8'))
        original = Path(report['test_data_directory']) / 'hub.db'
        db_path = root / 'probe.db'
        with sqlite3.connect(original) as source_db, sqlite3.connect(db_path) as db:
            source_db.backup(db)
            attempt, workflow = db.execute("SELECT id,workflow_id FROM project_attempts WHERE agent_id='dsh-win' AND stage='implement' LIMIT 1").fetchone()
            db.execute('UPDATE projects SET root=?', (str(root),))
            db.execute('UPDATE workflows SET status=?', ('running',))
            db.execute('UPDATE project_attempts SET status=? WHERE id=?', ('running', attempt))
            db.execute('UPDATE project_tasks SET worktree=NULL')
            db.execute('DELETE FROM project_leases')
            project_id = db.execute('SELECT project_id FROM workflows WHERE id=?', (workflow,)).fetchone()[0]
            db.execute('INSERT INTO project_leases(workflow_id,project_id,root_key,acquired_at) VALUES(?,?,?,?)', (workflow, project_id, str(root).replace('\\', '/').lower(), int(time.time() * 1000)))
        servers = [{'name': 'agent_hub', 'command': str(ROOT / 'release/Agent Hub.exe'), 'args': ['--project-tools', str(db_path), attempt], 'env': []}]
    env = os.environ.copy()
    env['AGENT_HUB_DSH_HOME'] = str(root / 'home')
    env['AGENT_HUB_DSH_PROJECT_MODE'] = '1'
    env['AGENT_HUB_DSH_DIAGNOSTICS'] = '1'
    env['AGENT_HUB_DSH_AUTH_BRIDGE'] = (ROOT / 'src-tauri/src/dsh_hermes_auth.py').read_text(encoding='utf-8')
    source = (ROOT / 'src-tauri/src/dsh_bridge.mjs').read_text(encoding='utf-8')
    proc = subprocess.Popen(['node', '--input-type=module', '-e', source, local_paths.dsh_installation(), str(root / 'native')],
                            cwd=root, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, encoding='utf-8', creationflags=subprocess.CREATE_NO_WINDOW)
    incoming = queue.Queue()
    updates = []
    client_requests = []
    def reader():
        for line in proc.stdout:
            try: incoming.put(json.loads(line))
            except ValueError: pass
        incoming.put(None)
    def diagnostics():
        for line in proc.stderr:
            try:
                data = json.loads(line)
                if isinstance(data, list):
                    projected = [{key: item.get(key) for key in ('errorClass', 'frames', 'knownTerms')} for item in data if isinstance(item, dict)]
                    if any(item.get('errorClass') or item.get('frames') or item.get('knownTerms') for item in projected):
                        print(json.dumps({'diagnostics': projected}), flush=True)
            except ValueError: pass
    threading.Thread(target=reader, daemon=True).start()
    threading.Thread(target=diagnostics, daemon=True).start()
    next_id = 0
    def write(value):
        proc.stdin.write(json.dumps({'jsonrpc': '2.0', **value}) + '\n')
        proc.stdin.flush()
    def rpc(method, params, timeout=300):
        nonlocal next_id
        next_id += 1
        current = next_id
        write({'id': current, 'method': method, 'params': params})
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try: value = incoming.get(timeout=1)
            except queue.Empty: continue
            if value is None: raise RuntimeError('DSH disconnected')
            if value.get('id') == current and not value.get('method'):
                if value.get('error'):
                    print(json.dumps({'rpc_error': safe_error(value['error'])}), flush=True)
                    return None
                return value.get('result')
            if value.get('method') == 'session/update': updates.append(value['params']['update'])
            elif 'id' in value and value.get('method') == 'session/request_permission':
                client_requests.append('session/request_permission')
                options = value['params'].get('options', [])
                allow = next((o for o in options if o.get('kind') in ('allow_once', 'allow_always')), None)
                if '--deny' in sys.argv: allow = None
                write({'id': value['id'], 'result': {'outcome': {'outcome': 'selected', 'optionId': allow['optionId']}} if allow else {'outcome': {'outcome': 'cancelled'}}})
        raise TimeoutError(method)
    try:
        rpc('initialize', {'protocolVersion': 1, 'clientCapabilities': {'fs': {'readTextFile': False, 'writeTextFile': False}, 'terminal': False}, 'clientInfo': {'name': 'tongxi-diagnostic', 'version': '1'}})
        session = rpc('session/new', {'cwd': str(root), 'mcpServers': servers})['sessionId']
        rpc('session/set_config_option', {'sessionId': session, 'configId': 'model', 'value': '["command-code-daily","deepseek/deepseek-v4.1-flash"]'})
        rpc('session/set_config_option', {'sessionId': session, 'configId': 'reasoning_effort', 'value': 'high'})
        prompt = '这是隔离验收。请使用文件工具读取当前目录的 calc.py，将 square(value) 改为返回 value 的平方，支持负数和零。只改这个文件，完成后简短确认。'
        if servers:
            prompt += '请使用 MCP hub_read 阅读源码，用 hub_message 向 hermes-win 交接结果，并用 hub_broadcast 向团队发布完成消息。'
        result = rpc('session/prompt', {'sessionId': session, 'prompt': [{'type': 'text', 'text': prompt}]})
        report = {'stop_reason': result.get('stopReason') if result else None,
                  'thought_chars': sum(len(u.get('content', {}).get('text', '')) for u in updates if u.get('sessionUpdate') == 'agent_thought_chunk'),
                  'message_chars': sum(len(u.get('content', {}).get('text', '')) for u in updates if u.get('sessionUpdate') == 'agent_message_chunk'),
                  'tool_events': sum(u.get('sessionUpdate') in ('tool_call', 'tool_call_update') for u in updates),
                  'client_requests': client_requests,
                  'file_changed': 'return value\n' not in (root / 'calc.py').read_text(encoding='utf-8')}
        print(json.dumps(report), flush=True)
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=10)


if __name__ == '__main__':
    run()
