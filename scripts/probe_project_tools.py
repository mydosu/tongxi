"""Real stdio broker checks in owned fixtures; no user projects or model calls.

Rust tests use the full Store schema. This transport fixture deliberately creates
only the tables consumed by the broker, to isolate protocol and tool behavior.
"""
import json
import os
from pathlib import Path
import queue
import sqlite3
import subprocess
import threading
import uuid

ROOT=Path(__file__).resolve().parents[1]
EXE=ROOT/'src-tauri/target/debug/local-agent-hub.exe'

def fixture(agent='codex-win'):
    base=ROOT/'artifacts'/('project-tools-probe-'+uuid.uuid4().hex[:10]);project=base/'project';data=base/'data'
    project.mkdir(parents=True);data.mkdir();db=data/'hub.db';attempt=str(uuid.uuid4());task=str(uuid.uuid4())
    with sqlite3.connect(db) as connection:
        connection.executescript('''PRAGMA user_version=11;
        CREATE TABLE projects(id TEXT PRIMARY KEY,root TEXT,checks TEXT);
        CREATE TABLE workflows(id TEXT PRIMARY KEY,project_id TEXT,status TEXT,plan TEXT);
        CREATE TABLE project_tasks(id TEXT PRIMARY KEY,files TEXT,worktree TEXT,assigned_agent TEXT);
        CREATE TABLE project_attempts(id TEXT PRIMARY KEY,workflow_id TEXT,task_id TEXT,agent_id TEXT DEFAULT 'codex-win',stage TEXT,status TEXT);
        CREATE TABLE project_leases(workflow_id TEXT PRIMARY KEY,root_key TEXT);
        CREATE TABLE project_changes(attempt_id TEXT,path TEXT,operation TEXT,before_hash TEXT,after_hash TEXT,PRIMARY KEY(attempt_id,path));''')
        connection.execute('INSERT INTO projects VALUES(?,?,?)',('project',str(project),json.dumps([{'name':'fixed test','program':'python','args':['check.py'],'timeout_seconds':10}])))
        connection.execute('INSERT INTO workflows VALUES(?,?,?,?)',('workflow','project','running',None))
        connection.execute('INSERT INTO project_tasks(id,files,worktree,assigned_agent) VALUES(?,?,?,?)',(task,json.dumps(['hello.txt','calc.py','src/new.txt','check.py']),None,None))
        connection.execute('INSERT INTO project_attempts VALUES(?,?,?,?,?,?)',(attempt,'workflow',task,'dsh-win','implement','running'))
        connection.execute('INSERT INTO project_leases VALUES(?,?)',('workflow',str(project).replace('\\','/').lower().rstrip('/')))
    (project/'check.py').write_text('# fixed verification script',encoding='utf-8')
    return {'base':base,'project':project,'data':data,'db':db,'attempt':attempt,'agent':agent}

class ToolSession:
    def __init__(self,context):
        self.context=context;self.identifier=0;self.messages=queue.Queue()
        self.process=subprocess.Popen([str(EXE),'--project-tools',str(context['db']),context['attempt']],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True,encoding='utf-8',creationflags=subprocess.CREATE_NO_WINDOW)
        threading.Thread(target=lambda:[self.messages.put(json.loads(line)) for line in self.process.stdout],daemon=True).start()
        self.rpc('initialize',{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'agent_hub_probe','version':'0.7.0'}})
        self.process.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized'})+'\n');self.process.stdin.flush()
    def rpc(self,method,params):
        self.identifier+=1;identifier=self.identifier
        self.process.stdin.write(json.dumps({'jsonrpc':'2.0','id':identifier,'method':method,'params':params},ensure_ascii=False)+'\n');self.process.stdin.flush()
        value=self.messages.get(timeout=20)
        if value.get('id')!=identifier or 'error' in value:raise RuntimeError('Scoped helper protocol failed')
        return value['result']
    def call(self,name,args):return self.rpc('tools/call',{'name':name,'arguments':args})
    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:self.process.terminate();self.process.wait(timeout=5)

def main():
    context=fixture();helper=ToolSession(context);checks=[]
    def check(name,condition):
        if not condition:raise AssertionError(name)
        checks.append(name)
    try:
        specs=helper.rpc('tools/list',{})['tools'];check('five scoped tools',len(specs)==5)
        written=helper.call('hub_write',{'path':'hello.txt','content':'BEFORE','expected_sha256':None});check('create real file',not written['isError'] and (context['project']/'hello.txt').read_text()=='BEFORE')
        read=helper.call('hub_read',{'path':'hello.txt'});contents=json.loads(read['content'][0]['text']);check('read SHA256',len(contents['sha256'])==64)
        wrong=helper.call('hub_write',{'path':'hello.txt','content':'CONFLICT','expected_sha256':None});check('hash conflict blocks overwrite',wrong['isError'] and (context['project']/'hello.txt').read_text()=='BEFORE')
        edited=helper.call('hub_edit',{'path':'hello.txt','old_text':'BEFORE','new_text':'AFTER','expected_sha256':contents['sha256']});check('guarded edit',not edited['isError'] and (context['project']/'hello.txt').read_text()=='AFTER')
        for path in ['../outside.txt','unscoped.txt','.env','hello.txt:stream']:
            response=helper.call('hub_write',{'path':path,'content':'NO','expected_sha256':None});check('deny '+path,response['isError'])
        before=helper.call('hub_read',{'path':'check.py'});sha=json.loads(before['content'][0]['text'])['sha256']
        response=helper.call('hub_write',{'path':'check.py','content':'PASS','expected_sha256':sha});check('fixed verification script protected',response['isError'])
        backups=context['data']/'project-backups'/context['attempt']
        check('new-file backup records original absence',any(json.loads(path.read_text())['existed'] is False for path in backups.glob('*.json')))
        (context['project']/'calc.py').write_text('ORIGINAL',encoding='utf-8')
        original=helper.call('hub_read',{'path':'calc.py'});sha=json.loads(original['content'][0]['text'])['sha256']
        written=helper.call('hub_write',{'path':'calc.py','content':'CHANGED','expected_sha256':sha})
        check('existing-file backup preserves original',not written['isError'] and any(path.read_bytes()==b'ORIGINAL' for path in backups.glob('*.bin')))
        with sqlite3.connect(context['db']) as db:db.execute("UPDATE workflows SET status='cancelling'")
        response=helper.call('hub_write',{'path':'src/new.txt','content':'STALE','expected_sha256':None});check('cancel rejects stale write',response['isError'] and not (context['project']/'src/new.txt').exists())
        with sqlite3.connect(context['db']) as db:
            db.execute("UPDATE workflows SET status='verifying'");db.execute("UPDATE project_attempts SET stage='verify'")
        response=helper.call('hub_write',{'path':'src/new.txt','content':'NO','expected_sha256':None});check('verify stage read only',response['isError'])
        response=helper.call('hub_read',{'path':'hello.txt'});check('verify read allowed',not response['isError'])
        with sqlite3.connect(context['db']) as db:db.execute('DELETE FROM project_leases')
        response=helper.call('hub_read',{'path':'hello.txt'});check('released lease rejects access',response['isError'])
        report={'passed':len(checks),'checks':checks,'fixture':str(context['base']),'fixture_scope':'minimal transport schema; full Store state tested in Rust','model_requests':0}
        (ROOT/'artifacts/project-tools-probe.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf-8');print(json.dumps({'passed':len(checks),'model_requests':0}))
    finally:helper.close()

if __name__=='__main__':main()
