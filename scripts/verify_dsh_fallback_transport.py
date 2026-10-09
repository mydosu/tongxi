"""Real installed DSH + isolated local HTTP routes; zero paid model requests."""
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import local_paths

ROOT=Path(__file__).resolve().parents[1]
counts={'one':0,'two':0}
def handler(route):
 class Handler(BaseHTTPRequestHandler):
  def log_message(self,*args): pass
  def do_POST(self):
   data=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
   counts[route]+=1
   if route=='one' and counts[route]>1:
    self.send_response(429);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(b'{"error":{"message":"Rate limit","type":"rate_limit"}}');return
   self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
   if route=='one':
    tool=next(tool['function'] for tool in data['tools'] if tool['function']['name']=='pwsh')
    props=tool['parameters']['properties'];key=next(key for key in ['command','script','code'] if key in props)
    args={key:"Add-Content -LiteralPath counter.txt -Value once"}
    for required in tool['parameters'].get('required',[]):
     if required not in args:args[required]='fixture' if props[required].get('type')=='string' else 30
    delta={'tool_calls':[{'index':0,'id':'call_owned_counter','type':'function','function':{'name':'pwsh','arguments':json.dumps(args)}}]};reason='tool_calls'
   else:delta={'content':'fallback_verified'};reason='stop'
   for change,finish in [(delta,None),({},reason)]:
    message={'id':'fixture-response','object':'chat.completion.chunk','model':'deepseek/deepseek-v4.1-flash','choices':[{'index':0,'delta':change,'finish_reason':finish}]}
    self.wfile.write(('data: '+json.dumps(message)+'\n\n').encode())
   self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush()
 return Handler

def main():
 root=Path(tempfile.mkdtemp(prefix='tongxi-fallback-'))
 servers=[ThreadingHTTPServer(('127.0.0.1',0),handler(route)) for route in ['one','two']]
 for server in servers:threading.Thread(target=server.serve_forever,daemon=True).start()
 routes=[{'id':provider,'name':name,'model':'deepseek/deepseek-v4.1-flash','baseURL':f'http://127.0.0.1:{server.server_port}/v1','apiKey':uuid.uuid4().hex,'headers':{}} for provider,name,server in zip(['command-code-daily','command-code-daily-2'],['Daily One fixture','Daily Two fixture'],servers)]
 env=os.environ.copy();env['AGENT_HUB_DSH_HOME']=str(root/'home');env['PYTHONIOENCODING']='utf-8'
 env['AGENT_HUB_DSH_AUTH_BRIDGE']='import json; print(json.dumps('+repr(routes)+'))'
 env['AGENT_HUB_DSH_FALLBACK_BRIDGE']=(ROOT/'src-tauri/src/dsh_fallback.mjs').read_text(encoding='utf-8')
 proc=subprocess.Popen(['node','--input-type=module','-e',(ROOT/'src-tauri/src/dsh_bridge.mjs').read_text(encoding='utf-8'),local_paths.dsh_installation(),str(root/'native')],cwd=root,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True,encoding='utf-8',creationflags=subprocess.CREATE_NO_WINDOW)
 incoming=queue.Queue();events=[];counter=0
 def reader():
  for line in proc.stdout:
   try:incoming.put(json.loads(line))
   except ValueError:pass
  incoming.put(None)
 threading.Thread(target=reader,daemon=True).start()
 def rpc(method,params):
  nonlocal counter
  counter+=1;proc.stdin.write(json.dumps({'jsonrpc':'2.0','id':counter,'method':method,'params':params})+'\n');proc.stdin.flush()
  while True:
   value=incoming.get(timeout=120)
   if value is None:raise RuntimeError('native bridge exited')
   if value.get('id')==counter and 'method' not in value:
    if value.get('error'):raise RuntimeError('native RPC failed, code '+str(value['error'].get('code')))
    return value['result']
   if value.get('method'):events.append(value)
 try:
  rpc('initialize',{'protocolVersion':1,'clientCapabilities':{'fs':{'readTextFile':False,'writeTextFile':False},'terminal':False}})
  session=rpc('session/new',{'cwd':str(root),'mcpServers':[]})['sessionId']
  result=rpc('session/prompt',{'sessionId':session,'prompt':[{'type':'text','text':'Run the fixture command once, then report completion.'}]})
  assert result['stopReason']=='end_turn'
  assert (root/'counter.txt').read_text().splitlines()==['once']
  assert counts=={'one':2,'two':1},counts
  notices=[event['params'] for event in events if event.get('method')=='agent_hub/model_route' and event['params'].get('fallback')]
  assert len(notices)==1 and notices[0]['sessionId']==session
  assert json.loads(notices[0]['modelId'])[0]=='command-code-daily-2'
  report={'passed':5,'primary_requests':counts['one'],'fallback_requests':counts['two'],'tool_executions':1,'session_identity_verified':True,'paid_model_requests':0}
  (ROOT/'artifacts/dsh-fallback-transport.json').write_text(json.dumps(report,indent=2),encoding='utf-8');print(json.dumps(report))
 finally:
  if proc.poll() is None:proc.terminate();proc.wait(timeout=10)
  for server in servers:server.shutdown()

if __name__=='__main__':main()
