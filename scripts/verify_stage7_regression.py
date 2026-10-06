"""Focused private/group/WSL regression after owned process lifetime change."""
import json
import sys
import uuid
from playwright.sync_api import sync_playwright
import smoke_desktop as desktop
import verify_stage3 as prior
import verify_stage4 as dsh
import verify_stage5 as albion
import verify_stage6 as groups
import probe_albion as probe
from stage7_evidence import begin_executable_run, finish_executable_run

desktop.EXE=desktop.ROOT/'release/candidate/v0.7.0/Agent Hub.exe'
desktop.DATA=desktop.ARTIFACTS/('stage7-regression-'+uuid.uuid4().hex[:10])
checks=[];errors=[];evidence=[]

def check(name,condition=True):
    if not condition:raise AssertionError(name)
    checks.append(name);print('PASS '+name,flush=True)

def run(resume=False):
    proc=None;success=False;before=probe.profile_hashes();gateway=albion.gateway_pids();executable_provenance=None if resume else begin_executable_run(desktop.EXE)
    if resume:
        report=json.loads((desktop.ARTIFACTS/'stage7-regression-verification.json').read_text(encoding='utf-8'))
        if report['passed']!=3:raise AssertionError('Only completed chat regression may resume WSL validation')
        if report.get('executable_sha256_before') and report.get('executable_sha256_after'):
            executable_provenance={'executable':str(desktop.EXE.resolve()),'executable_sha256_before':report['executable_sha256_before']}
        desktop.DATA=desktop.Path(report['test_data_directory']);checks.extend(report['checks']);evidence.extend(report['evidence'])
    try:
        with sync_playwright() as playwright:
            proc,endpoint=desktop.launch();browser=playwright.chromium.connect_over_cdp(endpoint);page=desktop.page_for(browser)
            page.on('pageerror',lambda error:errors.append(str(error)))
            if not resume:
                direct=prior.new_private(page,'dsh-win','项目工具隔离后的 DSH 私聊')
                page.locator('#connect-dsh').click();prior.wait_connection(page,'dsh');dsh.settings(page,None,'off')
                marker='CHAT_'+uuid.uuid4().hex[:8]
                reply=prior.wait_run(page,'dsh',prior.send(page,'dsh','不要调用工具。只原样回复 '+marker))
                native=dsh.native_dsh_evidence(reply)
                check('real DSH private chat retains zero native tools after project mode added',marker in reply['text'] and bool(native['tool_counts']) and all(count==0 for count in native['tool_counts']))
                evidence.append({'dsh_private':native})
                group=groups.group(page,'项目之后的群聊回归',['hermes-win','dsh-win'])
                page.locator('#connect-discussion').click();prior.wait_connection(page,'hermes');prior.wait_connection(page,'dsh')
                groups.live_settings(page,'hermes-win',None,'none');groups.live_settings(page,'dsh-win',None,'off')
                token='GROUP_'+uuid.uuid4().hex[:8]
                groups.send(page,group,'不要调用工具，每人只回复公开标记 '+token+'，后一个成员确认看到了前一个的回复。')
                finished=groups.wait_job(page,group,lambda j:j['status'] in ['completed','failed','interrupted'])
                detail=desktop.ipc(page,'get_conversation',id=group);texts={m['id']:m['content'] for m in detail['messages']}
                check('real two-member group remains operational with public context',finished['status']=='completed' and len(finished['turns'])==2 and all(token in texts[turn['assistant_message_id']] for turn in finished['turns']))
                group_dsh=next(turn for turn in finished['turns'] if turn['agent_id']=='dsh-win');native_group=dsh.native_dsh_evidence(group_dsh)
                check('real group DSH also retains zero tools and separate native session',bool(native_group['tool_counts']) and all(count==0 for count in native_group['tool_counts']) and group_dsh['native_thread_id']!=reply['native_thread_id'])
                evidence.append({'group':finished,'dsh_group':native_group})
            albion_private=prior.new_private(page,'albion-wsl','WSL 子进程生命周期回归')
            page.locator('#connect-albion').click();catalog=prior.wait_connection(page,'albion')
            dsh.settings(page,None,'none')
            marker='ALBION_'+uuid.uuid4().hex[:8]
            reply=prior.wait_run(page,'albion',prior.send(page,'albion','不要调用工具，请说出你的名字，然后原样回复 '+marker+'。最多60字。'))
            check('real Albion reply retains her persona after project engine changes',marker in reply['text'] and '阿尔比恩' in reply['text'])
            info=albion.native_info();pid=info['pid']
            check('WSL Albion preserves original SOUL identity',bool(catalog['models']) and info['identity_matched'] and info.get('relationship_matched') and info['soul_md5']==before[probe.PROFILE+'/SOUL.md'])
            desktop.ipc(page,'disconnect_albion');prior.wait_connection(page,'albion','disconnected')
            check('explicit disconnect terminates only the owned Albion Linux child',albion.wait_linux_exit(pid))
            page.locator('#connect-albion').click();prior.wait_connection(page,'albion');second=albion.native_info()
            check('Albion reconnect starts a fresh owned child',second['pid']!=pid)
            proc.terminate();proc.wait(timeout=10);proc=None
            check('hard Windows exit also terminates owned Albion Linux child',albion.wait_linux_exit(second['pid']))
            check('original Albion profile MD5 and gateway processes remain unchanged',probe.profile_hashes()==before and albion.gateway_pids()==gateway)
            check('no JavaScript runtime errors',not errors);success=True
    finally:
        if proc and proc.poll() is None:proc.terminate();proc.wait(timeout=10)
        report={'success':success,'passed':len(checks),'checks':checks,'javascript_errors':errors,'test_data_directory':str(desktop.DATA),'evidence':evidence,'original_profile_files_checked':len(before),'original_profile_files_changed':sum(probe.profile_hashes().get(p)!=h for p,h in before.items()),'gateway_before':gateway,'gateway_after':albion.gateway_pids()}
        if executable_provenance is not None:report.update(finish_executable_run(desktop.EXE,executable_provenance))
        (desktop.ARTIFACTS/'stage7-regression-verification.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf-8')

if __name__=='__main__':run('--resume-wsl' in sys.argv);print('Stage7 regression: '+str(len(checks))+' passed',flush=True)
