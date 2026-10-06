"""Read-only final source/package audit. Capture MD5 before final checks."""
import argparse
import hashlib
import json
from pathlib import Path
import re
from stage7_evidence import classify_report_provenance

ROOT=Path(__file__).resolve().parents[1]
ARTIFACTS=ROOT/'artifacts'
BASELINE=ARTIFACTS/'stage7-source-md5-before.json'

def targets():
    files=[]
    for directory in ['src','src-tauri/src','scripts']:
        files.extend(p for p in (ROOT/directory).rglob('*') if p.is_file() and p.suffix in ['.ts','.css','.rs','.py','.ps1','.mjs'])
    files.extend(p for p in ROOT.glob('*') if p.is_file() and p.suffix in ['.md','.json','.html','.ps1'])
    files.extend(ROOT/'src-tauri'/name for name in ['Cargo.toml','Cargo.lock','tauri.conf.json'])
    return sorted(set(files))

def md5():return {p.relative_to(ROOT).as_posix():hashlib.md5(p.read_bytes()).hexdigest() for p in targets()}

def final():
    before=json.loads(BASELINE.read_text(encoding='utf-8'));after=md5()
    changed=sum(after.get(p)!=value for p,value in before.items())+len(after.keys()-before.keys())
    checks=[]
    def check(name,condition):
        if not condition:raise AssertionError(name)
        checks.append(name);print('PASS '+name)
    check('final read-only checks leave every source target MD5 unchanged',changed==0)
    candidate=ROOT/'release/candidate/v0.7.0/Agent Hub.exe';binary=candidate.read_bytes();sha=hashlib.sha256(binary).hexdigest()
    report_names=['stage7-verification.json','stage7-cancel-verification.json','stage7-crash-verification.json','stage7-repair-verification.json','stage7-permanent-verification.json','stage7-native-verification.json','stage7-regression-verification.json']
    reports={};provenance={}
    for name in report_names:
        value=json.loads((ARTIFACTS/name).read_text(encoding='utf-8'))
        check(name+' succeeds without JavaScript errors',value['success'] and not value.get('javascript_errors'))
        reports[name]=value['passed']
        provenance[name]=classify_report_provenance(value,sha)
        if provenance[name]=='current-candidate':
            check(name+' executable path is the audited candidate',bool(value.get('executable')) and Path(value['executable']).resolve()==candidate.resolve())
    required_current=['stage7-permanent-verification.json','stage7-native-verification.json']
    for name in required_current:
        check(name+' is bound to the audited current candidate hash',provenance[name]=='current-candidate')
    check('no scenario report has partial or invalid executable provenance',all(value!='invalid-provenance' for value in provenance.values()))
    keys=json.loads((ARTIFACTS/'stage7-all-key-audit.json').read_text(encoding='utf-8'))
    check('two authorized route credentials never persisted in audited owned fixtures',keys['authorized_routes']==2 and keys['credential_matches']==0 and keys['owned_data_directories']==7)
    restore=json.loads((ARTIFACTS/'mwb-restoration.json').read_text(encoding='utf-8-sig'))
    check('temporary mouse sharing helper restored without settings changes',restore['helper_restored'] and not restore['settings_changed'])
    rust=(ARTIFACTS/'stage7-rust-current.txt').read_text(encoding='utf-8-sig')
    match=re.search(r'test result: ok\. (\d+) passed; 0 failed',rust)
    rust_tests_passed=int(match.group(1)) if match else 0
    check('all current Rust tests pass at the required minimum',bool(match) and rust_tests_passed>=62)
    checks_exit=json.loads((ARTIFACTS/'checks-exit.json').read_text(encoding='utf-8'))
    expected_commands={
        'clippy':'cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings',
        'rust_tests':'cargo test --manifest-path src-tauri/Cargo.toml',
        'frontend':'npm run build',
        'fmt':'cargo fmt --manifest-path src-tauri/Cargo.toml --check',
    }
    for name,command in expected_commands.items():
        check('current '+name+' command has exact command and zero exit status',checks_exit[name]['exactcommand']==command and checks_exit[name]['exit_code']==0)
    check('candidate matches final compiled release binary',binary==(ROOT/'src-tauri/target/release/local-agent-hub.exe').read_bytes())
    v6='76524946829144f720e35341257d4c92551b624ebbb03de8cc79bab885f4722f'
    check('previous stable v6 package retained before publication',hashlib.sha256((ROOT/'release/Agent Hub.exe').read_bytes()).hexdigest()==v6 and hashlib.sha256((ROOT/'release/v0.6.0/Agent Hub.exe').read_bytes()).hexdigest()==v6)
    check('source manifests consistently declare 0.7.0',json.loads((ROOT/'package.json').read_text())['version']=='0.7.0' and json.loads((ROOT/'src-tauri/tauri.conf.json').read_text())['version']=='0.7.0' and 'version = "0.7.0"' in (ROOT/'src-tauri/Cargo.toml').read_text())
    result={'success':True,'passed':len(checks),'checks':checks,'source_files_checked':len(before),'source_files_changed':changed,'rust_tests_passed':rust_tests_passed,'scenario_checks':reports,'scenario_executable_provenance':provenance,'legacy_unbound_scenarios':[name for name,state in provenance.items() if state=='legacy-unbound'],'different_candidate_scenarios':[name for name,state in provenance.items() if state=='different-candidate'],'invalid_provenance_scenarios':[name for name,state in provenance.items() if state=='invalid-provenance'],'candidate_bytes':len(binary),'candidate_sha256':sha,'credential_audit':keys,'mwb_restored':True,'publication':'candidate accepted, stable v6 retained at audit time','model_requests':0}
    (ARTIFACTS/'stage7-final-audit.json').write_text(json.dumps(result,ensure_ascii=False,indent=2),encoding='utf-8')
    print(json.dumps({k:v for k,v in result.items() if k not in ['checks','scenario_checks','credential_audit']}))

if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--capture',action='store_true');args=parser.parse_args()
    if args.capture:
        baseline=md5();BASELINE.write_text(json.dumps(baseline,ensure_ascii=False,indent=2),encoding='utf-8');print('Captured source MD5: '+str(len(baseline)))
    else:final()
