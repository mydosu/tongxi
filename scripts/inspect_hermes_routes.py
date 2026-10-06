"""Print only provider labels and model identifiers through Hermes read-only loader."""
import os
from pathlib import Path
import sys
repo=Path(os.environ.get('LOCALAPPDATA',''))/'hermes/hermes-agent'
sys.path.insert(0,str(repo))
os.environ['HERMES_HOME']=str(repo.parent)
import hermes_bootstrap
hermes_bootstrap.harden_import_path()
from hermes_cli.config import load_config_readonly, get_compatible_custom_providers
import json
config=load_config_readonly()
providers=get_compatible_custom_providers(config)
if isinstance(providers,dict): providers=list(providers.values())
for provider in providers:
    label=str(provider.get('name',''))
    models=provider.get('models',[]) or []
    ids=[entry if isinstance(entry,str) else entry.get('model') or entry.get('id') or entry.get('name') for entry in models]
    print(json.dumps({'name':label,'provider_key':provider.get('provider_key'),'model':provider.get('model'),'models':ids},ensure_ascii=True))
aliases=config.get('model_aliases',{})
for name,value in aliases.items():
    if isinstance(value,dict):
        print(json.dumps({'alias':name,'provider':value.get('provider'),'model':value.get('model')},ensure_ascii=True))
