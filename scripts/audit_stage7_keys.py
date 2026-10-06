"""Scan only this phase's isolated data for exact authorized route credentials.

Credentials travel through a private subprocess pipe and stay in memory. Output
is limited to counts; no key, hash, config or provider error is ever printed.
"""
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT=Path(__file__).resolve().parents[1]
names=['stage7-verification.json']
if '--all' in sys.argv:
    names += ['stage7-'+scenario+'-verification.json' for scenario in ['cancel','crash','repair','permanent','native','regression']]
directories=[]
for name in names:
    report=json.loads((ROOT/'artifacts'/name).read_text(encoding='utf-8'))
    if not report.get('success'):raise RuntimeError('Scenario must complete before final key audit')
    data=Path(report['test_data_directory']).resolve()
    if data.parent!=(ROOT/'artifacts').resolve() or not data.name.startswith('stage7-'):raise RuntimeError('Audit scope is not an owned phase fixture')
    if data not in directories:directories.append(data)
repo=Path(os.environ.get('AGENT_HUB_DSH_HERMES_REPO',str(Path(os.environ['LOCALAPPDATA'])/'hermes/hermes-agent')))
python=Path(os.environ.get('AGENT_HUB_HERMES_PYTHON',str(repo/'.venv/Scripts/python.exe')))
source=(ROOT/'src-tauri/src/dsh_hermes_auth.py').read_text(encoding='utf-8')
resolved=subprocess.run([str(python),'-u','-c',source,str(repo),'--runtime'],capture_output=True,encoding='utf-8',timeout=45,creationflags=subprocess.CREATE_NO_WINDOW)
if resolved.returncode:raise RuntimeError('Authorized route resolution failed; diagnostics suppressed')
routes=json.loads(resolved.stdout)
if len(routes)!=2:raise RuntimeError('Expected two authorized routes')
secrets=[route['apiKey'].encode() for route in routes if isinstance(route.get('apiKey'),str) and len(route['apiKey'])>=12]
if len(secrets)!=2:raise RuntimeError('Credential audit input unavailable')
count=0;matches=0
for data in directories:
    for path in data.rglob('*'):
        if not path.is_file():continue
        count+=1
        with path.open('rb') as reader:
            tail=b''
            while chunk:=reader.read(1_048_576):
                combined=tail+chunk
                if any(secret in combined for secret in secrets):matches+=1;break
                tail=combined[-max(map(len,secrets)):]
result={'owned_files_scanned':count,'owned_data_directories':len(directories),'authorized_routes':2,'credential_matches':matches,'scope':'completed phase7 isolated application data only','model_requests':0}
target='stage7-all-key-audit.json' if '--all' in sys.argv else 'stage7-key-audit.json'
(ROOT/'artifacts'/target).write_text(json.dumps(result,ensure_ascii=False,indent=2),encoding='utf-8')
print(json.dumps(result))
if matches:raise RuntimeError('Credential persistence detected; values suppressed')
