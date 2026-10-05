"""Resolve two authorized Hermes routes. Runtime output is private child IPC only.

Default invocation prints safe metadata. --runtime is consumed directly by the
owned Node process; never invoke that mode from a shell or save its output.
"""
import contextlib
import io
import json
import logging
import os
from pathlib import Path
import sys

repo=Path(sys.argv[1]).resolve()
sys.path.insert(0,str(repo))
os.environ['HERMES_HOME']=str(repo.parent)
logging.disable(logging.CRITICAL)
try:
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
        import hermes_bootstrap
        hermes_bootstrap.harden_import_path()
        from acp_adapter.entry import _load_env
        _load_env()
        from hermes_cli import config as config_module
        from hermes_cli.runtime_provider import resolve_runtime_provider
        config_module.load_config=config_module.load_config_readonly
        config=config_module.load_config_readonly()
        providers=config_module.get_compatible_custom_providers(config)
        routes=[]
        configured_keys={entry.get('provider_key') for entry in providers}
        # Hermes renamed the first daily route to `command-code-daily-1` in newer
        # configs. Keep Agent Hub's stable route id while resolving either Hermes
        # spelling, preferring the current name when both are present.
        for route_id,candidates in [
            ('command-code-daily',['command-code-daily-1','command-code-daily']),
            ('command-code-daily-2',['command-code-daily-2']),
        ]:
            key=next((candidate for candidate in candidates if candidate in configured_keys),None)
            provider=next((entry for entry in providers if entry.get('provider_key')==key),None)
            if provider is None: raise RuntimeError('Required route unavailable')
            model='deepseek/deepseek-v4.1-flash'
            runtime=resolve_runtime_provider(requested=key,target_model=model)
            token=runtime.get('api_key')
            if callable(token): token=token()
            if not token or not runtime.get('base_url'): raise RuntimeError('Route unavailable')
            routes.append({'id':route_id,'name':provider.get('name',key),'model':model,
                'baseURL':runtime['base_url'],'apiKey':str(token),
                'headers':runtime.get('extra_headers') or {}})
        if '--models' in sys.argv:
            from openai import OpenAI
            for route in routes:
                try:
                    models=OpenAI(api_key=route['apiKey'],base_url=route['baseURL'],default_headers=route['headers'],timeout=20,max_retries=0).models.list()
                    route['availableModels']=[model.id for model in models if 'deepseek' in model.id.lower() and '4.1' in model.id]
                except Exception:
                    route['availableModels']=None
    if '--runtime' in sys.argv:
        # Secret-bearing output only to execFile's bounded private pipe.
        sys.stdout.write(json.dumps(routes))
    else:
        print(json.dumps([{key:route.get(key) for key in ['id','name','model','availableModels']} for route in routes],ensure_ascii=True))
except Exception:
    sys.stderr.write('Hermes Command Code routes unavailable\n')
    sys.exit(1)
