"""Safe native ACP probe. No model prompts and no raw native diagnostics."""
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import uuid
import sys
import local_paths

ROOT = Path(__file__).resolve().parents[1]

class Client:
    def __init__(self, data):
        script=(ROOT/'src-tauri/src/dsh_bridge.mjs').read_text(encoding='utf-8')
        installation=local_paths.dsh_installation()
        env=os.environ.copy(); env['AGENT_HUB_DSH_DIAGNOSTICS']='1'
        env['AGENT_HUB_DSH_AUTH_BRIDGE']=(ROOT/'src-tauri/src/dsh_hermes_auth.py').read_text(encoding='utf-8')
        self.proc=subprocess.Popen(['node','--input-type=module','-e',script,installation,str(data)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',env=env,creationflags=subprocess.CREATE_NO_WINDOW)
        self.messages=queue.Queue(); self.next=0; self.updates=[]
        def reader():
            for line in self.proc.stdout:
                try: self.messages.put(json.loads(line))
                except ValueError: pass
            self.messages.put(None)
        threading.Thread(target=reader,daemon=True).start()
        def diagnostics():
            for line in self.proc.stderr:
                try:
                    value=json.loads(line)
                    if isinstance(value,list) and any(item.get('frames') or item.get('knownTerms') or item.get('errorClass') for item in value): print({'native_sanitized_diagnostics':value},flush=True)
                except ValueError: pass
        threading.Thread(target=diagnostics,daemon=True).start()
    def rpc(self, method, params, timeout=70):
        self.next+=1; id=self.next
        self.proc.stdin.write(json.dumps({'jsonrpc':'2.0','id':id,'method':method,'params':params})+'\n'); self.proc.stdin.flush()
        while True:
            result=self.messages.get(timeout=timeout)
            if result is None: raise RuntimeError('Native DSH exited')
            if result.get('method'): self.updates.append(result); continue
            if result.get('id')==id:
                if 'error' in result:
                    # Diagnose only known fixed phrases, never echo native text.
                    message=json.dumps(result['error'])
                    phrases=['workspace','model','provider','credentials','cannot read properties','not found','not registered','missing','persist','agent','scope','fs','sandbox','is not a function','requires','api','key','unauthorized','auth','http','401','402','403','404','429','fetch','failed','timeout','connection','network','request','function','configure','protocol','max','token','session','queued','cancel','reasoning','thinking','undefined']
                    print({'error_code':result['error'].get('code'),'known_diagnostic_terms':[word for word in phrases if word in message.lower()]})
                    import re
                    print({'native_modules':re.findall(r'@deepseek-ai/[a-z0-9-]+',message),'fixed_error_terms':[word for word in ['Unknown','unknown','Undefined','undefined','Error','inactive','workdir','selection','request','serial','namespace','systemPrompt','sessionQuery','sessionProjection','subprocess','ENOENT','tools','path','resolve','serialize','restore','sessionId'] if word in message]})
                    raise RuntimeError('Native protocol failure: '+str(result['error'].get('code')))
                return result.get('result',{})
    def close(self):
        self.proc.terminate(); self.proc.wait(timeout=10)

def run():
    data=ROOT/'artifacts'/('dsh-probe-'+uuid.uuid4().hex[:10]); data.mkdir(parents=True)
    client=Client(data)
    try:
        init=client.rpc('initialize',{'protocolVersion':1,'clientCapabilities':{},'clientInfo':{'name':'agent-hub-probe','version':'0.4.0'}})
        response=client.rpc('session/new',{'cwd':str(data),'mcpServers':[]})
        options=response.get('configOptions',[])
        # Whitelist only non-sensitive ACP option fields.
        print(json.dumps({'version':init.get('agentInfo',{}).get('version'),'session':bool(response.get('sessionId')),'configOptions':options},ensure_ascii=True),flush=True)
        if '--prompt' in sys.argv:
            client.rpc('session/set_config_option',{'sessionId':response['sessionId'],'configId':'reasoning_effort','value':'off'})
            marker='DSH_PROBE_'+uuid.uuid4().hex[:8]
            result=client.rpc('session/prompt',{'sessionId':response['sessionId'],'prompt':[{'type':'text','text':'只回复 '+marker+'。'}]},timeout=180)
            text=''.join(update.get('params',{}).get('update',{}).get('content',{}).get('text','') for update in client.updates if update.get('params',{}).get('update',{}).get('sessionUpdate')=='agent_message_chunk')
            success=result.get('stopReason')=='end_turn' and marker in text
            print({'real_prompt':success,'assistant_characters':len(text)},flush=True)
            if not success: raise AssertionError('Native DSH reply did not match')
    finally: client.close()

if __name__=='__main__': run()
