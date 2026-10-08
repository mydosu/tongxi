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
                    print({'error_code':result['error'].get('code'),'error_fields':sorted(result['error'].keys()),'message_present':bool(result['error'].get('message')),'message_characters':len(result['error'].get('message','')),'data_type':type(result['error'].get('data')).__name__,'known_diagnostic_terms':[word for word in phrases if word in message.lower()]})
                    import re
                    safe_message=result['error'].get('message','')
                    safe_message=re.sub(r'(?i)(?:bearer\s+|(?:api[_-]?key|token|secret)\s*[:=]\s*)[^\s,;]+','[redacted]',safe_message)
                    safe_message=re.sub(r'\b[A-Za-z0-9_-]{32,}\b','[redacted]',safe_message)
                    print({'sanitized_error_message':safe_message})
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
            # Explicitly pin the real probe to DSH's Daily One / DS V4.1 route; no Sol model is used.
            client.rpc('session/set_config_option',{'sessionId':response['sessionId'],'configId':'model','value':'[\"command-code-daily\",\"deepseek/deepseek-v4.1-flash\"]'})
            effort='high' if '--high' in sys.argv else 'off'
            client.rpc('session/set_config_option',{'sessionId':response['sessionId'],'configId':'reasoning_effort','value':effort})
            marker='DSH_TOOL_'+uuid.uuid4().hex[:8]
            filename='agent-hub-tool-probe.txt'
            result=client.rpc('session/prompt',{'sessionId':response['sessionId'],'prompt':[{'type':'text','text':'请使用你在 DSH CLI 中可用的文件或终端工具，在当前工作目录创建文件 '+filename+'，文件内容必须精确为 '+marker+'。工具操作完成后简短确认。'}]},timeout=180)
            text=''.join(update.get('params',{}).get('update',{}).get('content',{}).get('text','') for update in client.updates if update.get('params',{}).get('update',{}).get('sessionUpdate')=='agent_message_chunk')
            target=data/filename
            success=result.get('stopReason')=='end_turn' and target.is_file() and target.read_text(encoding='utf-8')==marker
            print({'real_tool_prompt':success,'tool_file_created':target.is_file(),'assistant_characters':len(text),'provider':'command-code-daily','model':'deepseek/deepseek-v4.1-flash','reasoning_effort':effort},flush=True)
            if not success: raise AssertionError('DSH native tool did not create the isolated fixture file')
    finally: client.close()

if __name__=='__main__': run()
