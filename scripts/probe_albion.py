"""Probe WSL Albion ACP without model calls or user-history access."""
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import threading
import uuid

ROOT=Path(__file__).resolve().parents[1]
DISTRO=os.environ.get('AGENT_HUB_ALBION_DISTRO','Ubuntu')
REPO=os.environ.get('AGENT_HUB_ALBION_REPO','/usr/local/lib/hermes-agent')
PROFILE=os.environ.get('AGENT_HUB_ALBION_PROFILE','/root/.hermes/profiles/albion')
PYTHON=os.environ.get('AGENT_HUB_ALBION_PYTHON',REPO+'/venv/bin/python')
DATA=ROOT/'artifacts'/('albion-probe-'+uuid.uuid4().hex[:10])
DATA.mkdir(parents=True)
LINUX_DATA='/root/.local/share/local-agent-hub/probes/'+DATA.name

def profile_hashes():
    code="""import hashlib,json,pathlib,sys
root=pathlib.Path(sys.argv[1]); files=[root/'config.yaml',root/'.env',root/'SOUL.md']
print(json.dumps({str(p):hashlib.md5(p.read_bytes()).hexdigest() for p in files if p.is_file()}))
"""
    result=subprocess.run(['wsl','-d',DISTRO,'--exec',PYTHON,'-c',code,PROFILE],capture_output=True,text=True,encoding='utf-8',timeout=30,creationflags=subprocess.CREATE_NO_WINDOW)
    if result.returncode: raise RuntimeError('WSL profile inspection failed')
    return json.loads(result.stdout)

def run():
    baseline=profile_hashes(); messages=queue.Queue(); proc=None; report={'success':False,'model_requests':0}; diagnostics=[]; updates=[]
    # Pin profile before native imports; use the same tool-free chat bridge.
    prelude=(ROOT/'src-tauri/src/albion_prefix.py').read_text(encoding='utf-8')+'\n'
    source=prelude+(ROOT/'src-tauri/src/hermes_bridge.py').read_text(encoding='utf-8')
    try:
        proc=subprocess.Popen(['wsl','-d',DISTRO,'--exec',PYTHON,'-u','-c',source,REPO,LINUX_DATA,PROFILE],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',creationflags=subprocess.CREATE_NO_WINDOW)
        def safe_diagnostics():
            for line in proc.stderr:
                if line.startswith('AGENT_HUB_ALBION:'):
                    try: diagnostics.append(json.loads(line.split(':',1)[1]))
                    except ValueError: pass
        threading.Thread(target=safe_diagnostics,daemon=True).start()
        def reader():
            for line in proc.stdout:
                try: messages.put(json.loads(line))
                except ValueError: pass
            messages.put({'eof':True})
        threading.Thread(target=reader,daemon=True).start()
        def rpc(identifier,method,params):
            proc.stdin.write(json.dumps({'jsonrpc':'2.0','id':identifier,'method':method,'params':params})+'\n'); proc.stdin.flush()
            while True:
                reply=messages.get(timeout=60)
                if reply.get('eof'): raise RuntimeError('WSL ACP exited')
                if reply.get('id')!=identifier:
                    updates.append(reply)
                    continue
                if 'error' in reply: raise RuntimeError('WSL ACP request failed: '+method)
                return reply['result']
        initialized=rpc(1,'initialize',{'protocolVersion':1,'clientCapabilities':{'fs':{'readTextFile':False,'writeTextFile':False},'terminal':False},'clientInfo':{'name':'agent-hub-albion-probe','version':'0.4.0'}})
        created=rpc(2,'session/new',{'cwd':LINUX_DATA,'mcpServers':[]})
        applied=rpc(3,'session/set_config_option',{'sessionId':created['sessionId'],'configId':'reasoning_effort','value':'default'})
        if applied.get('_meta',{}).get('agentHub',{}).get('toolCount')!=0: raise RuntimeError('WSL chat tools are not isolated')
        info=initialized.get('agentInfo',{})
        report.update(success=True,handshake='ok',native_name=info.get('name'),version=info.get('version'),profile=PROFILE,owned_data=str(DATA),native_session_created=True,tools=0,model_count=len(created.get('models',{}).get('availableModels',[])))
        if '--prompt' in sys.argv:
            marker='ALBION_PROBE_'+uuid.uuid4().hex[:8]
            report['model_requests']=1
            result=rpc(4,'session/prompt',{'sessionId':created['sessionId'],'prompt':[{'type':'text','text':'不要调用工具，请用一句话自我介绍，然后原样输出 '+marker+'。'}]})
            text=''.join(u.get('params',{}).get('update',{}).get('content',{}).get('text','') for u in updates if u.get('params',{}).get('update',{}).get('sessionUpdate')=='agent_message_chunk')
            native=json.loads((Path('//wsl.localhost')/DISTRO/LINUX_DATA.lstrip('/')/'runtime-info.json').read_text(encoding='utf-8'))
            report.update(stop_reason=result.get('stopReason'),reply_marker=marker in text,name_detected='阿尔比恩' in text,identity_matched=native['identity_matched'],relationship_matched=native.get('relationship_matched'),reply_characters=len(text))
            report['success']=all(report[k] for k in ['reply_marker','name_detected','identity_matched','relationship_matched'])
    finally:
        if proc:
            proc.stdin.close()
            try: proc.wait(timeout=10)
            except subprocess.TimeoutExpired: proc.terminate(); proc.wait(timeout=10)
        after=profile_hashes()
        report['safe_diagnostics']=diagnostics
        if diagnostics: print(json.dumps({'safe_diagnostics':diagnostics}))
        report.update(profile_files_checked=len(baseline),profile_files_changed=sum(after.get(p)!=h for p,h in baseline.items()))
        if report['profile_files_changed']: report['success']=False
        (ROOT/'artifacts/albion-probe.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf-8')
    if not report['success']: raise RuntimeError('WSL profile changed during probe')
    print(json.dumps(report,ensure_ascii=False),flush=True)

if __name__=='__main__': run()
