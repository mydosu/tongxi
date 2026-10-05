"""WSL private-child setup: pin Albion identity, keep source profile read-only.

This prelude is followed by hermes_bridge.py in the same owned Python process.
argv: installed Hermes repo, owned native data, original Albion profile.
"""
import copy
import hashlib
import json
import os
from pathlib import Path
import sys
import select
import threading

_albion_repo=Path(sys.argv[1]).resolve()
_albion_data=Path(sys.argv[2]).resolve()
_albion_profile=Path(sys.argv[3]).resolve()
os.environ['HERMES_HOME']=str(_albion_profile)
os.environ['PYTHONDONTWRITEBYTECODE']='1'
sys.dont_write_bytecode=True
sys.path.insert(0,str(_albion_repo))
if not (_albion_profile/'config.yaml').is_file() or not (_albion_profile/'SOUL.md').is_file():
    raise RuntimeError('Albion profile and SOUL are required')
_albion_data.mkdir(parents=True,exist_ok=True)
_albion_soul=(_albion_profile/'SOUL.md').read_text(encoding='utf-8').strip()
_albion_relationship='同席会话：沿用 SOUL.md 里定义的人格与语气，不修改源人格文件。当前是独立会话，可能为私聊或受邀群内发言；只使用该会话收到的信息，不引用其他私人聊天，工具执行关闭。'
_albion_identity_hash=hashlib.md5((_albion_profile/'SOUL.md').read_bytes()).hexdigest()
_albion_info={'pid':os.getpid(),'profile':str(_albion_profile),'soul_md5':_albion_identity_hash,'identity_matched':False,'tool_count':0,'digest_injected':False}

def _albion_save_info():
    temporary=_albion_data/'runtime-info.json.tmp'
    temporary.write_text(json.dumps(_albion_info),encoding='utf-8')
    temporary.replace(_albion_data/'runtime-info.json')

_albion_save_info()

# WSL processes are outside Windows Job Objects. Close only this Linux child
# when its owning stdio pipe disappears, including app crashes during a turn.
def _albion_owner_watch():
    watcher=select.poll()
    watcher.register(sys.stdin.fileno(),select.POLLHUP|select.POLLERR)
    while True:
        if watcher.poll(500): os._exit(0)

threading.Thread(target=_albion_owner_watch,daemon=True).start()

# 会话库是唯一允许写的例外：同席要能看到并接着聊她自己的历史会话。
# 其余 profile/repo 文件（config.yaml、SOUL.md、代码……）依旧只读。
# 与桥同一套优先级：命令行参数 > 环境变量 > 她 profile 的库。
# 只把**真正在用**的那一个库放进白名单（隔离模式下就只放行隔离库）。
_albion_explicit_db=(sys.argv[4].strip() if len(sys.argv)>4 and sys.argv[4].strip() else None)
_albion_session_db=Path(_albion_explicit_db or os.environ.get('AGENT_HUB_HERMES_SESSION_DB') or (_albion_profile/'state.db')).resolve()
_albion_session_sidecars={Path(str(_albion_session_db)+suffix) for suffix in ('-wal','-shm','-journal')}

def _albion_normalize(value):
    # sqlite3.connect 可能收到 "file:...?mode=ro" 这种 URI，也可能是 Path/str；
    # 统一成绝对路径再比对，否则白名单认不出来（列表查询就是走 URI 的）。
    try:
        text=os.fsdecode(value)
    except Exception:
        return None
    if text.startswith('file:'):
        text=text[5:].split('?',1)[0]
    try:
        return Path(text).resolve()
    except Exception:
        return None

def _albion_is_session_db(path):
    if path is None: return False
    return path in _albion_session_sidecars or path==_albion_session_db

def _albion_protected(value):
    if not isinstance(value,(str,bytes,os.PathLike)): return False
    path=_albion_normalize(value)
    if _albion_is_session_db(path): return False
    if path is None: return False
    return any(path==root or root in path.parents for root in (_albion_profile,_albion_repo))

def _albion_audit(event,args):
    if event=='open' and _albion_protected(args[0]):
        mode=args[1] or ''
        flags=args[2] or 0
        if any(c in mode for c in 'wax+') or flags & (os.O_WRONLY|os.O_RDWR|os.O_CREAT|os.O_TRUNC|os.O_APPEND):
            sys.stderr.write('AGENT_HUB_ALBION:'+json.dumps({'blocked_event':event,'path':os.fsdecode(args[0])[:200]})+'\n')
            raise PermissionError('Source profile is read-only in Agent Hub')
    elif event=='sqlite3.connect' and _albion_protected(args[0]):
        sys.stderr.write('AGENT_HUB_ALBION:'+json.dumps({'blocked_event':event,'path':os.fsdecode(args[0])[:200]})+'\n')
        raise PermissionError('Use the owned Agent Hub session database')
    elif event in ('os.remove','os.rename','os.rmdir','os.chmod','os.chown','os.truncate','os.utime','os.symlink','os.link'):
        if any(_albion_protected(value) for value in args[:2]):
            raise PermissionError('Source profile is read-only in Agent Hub')
    elif event=='os.mkdir' and _albion_protected(args[0]) and not Path(args[0]).is_dir():
        raise PermissionError('Source profile is read-only in Agent Hub')

sys.addaudithook(_albion_audit)
import hermes_bootstrap
hermes_bootstrap.harden_import_path()
from hermes_cli import config as _albion_config
_albion_config.load_config=lambda: copy.deepcopy(_albion_config.load_config_readonly())
import hermes_logging as _albion_logging
# hermes_bridge.py installs a redacting stderr handler; no profile file logs.
_albion_logging.setup_logging=lambda *args,**kwargs: _albion_data
_albion_logging.setup_verbose_logging=lambda *args,**kwargs: None
# Existing voice/service plugins belong to the original running gateway.
import hermes_cli.plugins as _albion_plugins
_albion_plugins.discover_plugins=lambda *args,**kwargs: None
_albion_plugins.start_background_plugin_discovery=lambda *args,**kwargs: None
from run_agent import AIAgent as _AlbionNativeAgent
_albion_original_init=_AlbionNativeAgent.__init__

def _albion_init(self,*args,**kwargs):
    kwargs.update(skip_memory=True,skip_background_review=True,skip_context_files=True,load_soul_identity=True,checkpoints_enabled=False)
    try:
        _albion_original_init(self,*args,**kwargs)
    except Exception as error:
        frames=[]; trace=error.__traceback__
        while trace:
            frames.append({'module':Path(trace.tb_frame.f_code.co_filename).name,'line':trace.tb_lineno})
            trace=trace.tb_next
        sys.stderr.write('AGENT_HUB_ALBION:'+json.dumps({'exception_class':type(error).__name__,'frames':frames})+'\n')
        raise
    self.logs_dir=_albion_data/'request-logs'
    self.logs_dir.mkdir(parents=True,exist_ok=True)
    # Development digests are strictly opt-in: a fresh Agent starts with an
    # empty ephemeral slot, so a leftover dev-digest.txt is never injected.
    self.ephemeral_system_prompt=None

_AlbionNativeAgent.__init__=_albion_init
_albion_original_prompt=_AlbionNativeAgent._build_system_prompt

def _albion_prompt(self,*args,**kwargs):
    # Hermes otherwise derives identity home from SessionDB's parent. The Hub
    # deliberately stores that DB elsewhere, so bind the original profile here.
    from hermes_constants import set_hermes_home_override,reset_hermes_home_override
    token=set_hermes_home_override(_albion_profile)
    try: prompt=_albion_original_prompt(self,*args,**kwargs)
    finally: reset_hermes_home_override(token)
    prompt=prompt+'\n\n'+_albion_relationship
    _albion_info['identity_matched']=bool(_albion_soul) and _albion_soul[:min(300,len(_albion_soul))] in prompt
    _albion_info['relationship_matched']=_albion_relationship in prompt
    # No digest is read or injected while building the base prompt: the
    # per-session opt-in text lives in the ephemeral system slot only.
    _albion_save_info()
    return prompt

_AlbionNativeAgent._build_system_prompt=_albion_prompt

def _hub_set_digest(self,text):
    # Per-session development digest, opt-in. Hermes merges
    # self.ephemeral_system_prompt into the effective system prompt on every
    # API call, so the text never enters state.history and is never written to
    # a persistent cached prompt. Empty text clears it.
    if not getattr(self,'_hub_digest_primed',False):
        # Drop any cached base prompt that may carry an inherited digest.
        # Later ephemeral updates replace only the ephemeral slot, so the base
        # never needs rebuilding again.
        self._hub_digest_primed=True
        invalidate=getattr(self,'_invalidate_system_prompt',None)
        if callable(invalidate):
            invalidate()
        else:
            self._cached_system_prompt=None
            self._cached_system_prompt_static=None
    text=text.strip() if isinstance(text,str) else ''
    self.ephemeral_system_prompt=text or None
    _albion_info['digest_injected']=bool(text)
    _albion_save_info()
    return bool(text)

_AlbionNativeAgent._hub_set_digest=_hub_set_digest
