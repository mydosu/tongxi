"""Deterministic project navigation, model configuration, and transcript UI checks.

Synthetic IPC fixtures only; no model requests. Owns and closes its Vite runtime.
"""
import json
from pathlib import Path
import subprocess
import time
import urllib.request
from playwright.sync_api import sync_playwright, expect
from verify_model_provider_ui import MOCK

ROOT=Path(__file__).resolve().parents[1]
checks=[]

def check(name,condition=True):
    if not condition: raise AssertionError(name)
    checks.append(name); print('PASS '+name,flush=True)

def main():
    server=subprocess.Popen(['node',str(ROOT/'node_modules/vite/bin/vite.js'),'--host','127.0.0.1','--port','1420','--strictPort'],cwd=ROOT,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,creationflags=subprocess.CREATE_NO_WINDOW)
    success=False
    try:
        for _ in range(60):
            try:
                urllib.request.urlopen('http://127.0.0.1:1420',timeout=.3).close();break
            except OSError: time.sleep(.2)
        with sync_playwright() as p:
            browser=p.chromium.launch(headless=True)
            page=browser.new_page(viewport={'width':1440,'height':1000})
            errors=[];page.on('pageerror',lambda error:errors.append(str(error)))
            page.add_init_script(MOCK)
            page.goto('http://127.0.0.1:1420',wait_until='networkidle')
            expect(page.locator('[data-project-module]')).to_have_count(5)
            check('sidebar has team, progress, and three members')
            parent=page.locator('[data-conversation="provider-test"]')
            parent.click();expect(page.locator('[data-project-module="team"]')).to_be_hidden()
            parent.click();expect(page.locator('[data-project-module="team"]')).to_be_visible()
            check('parent button toggles sidebar with no separate expand control',not page.locator('[data-project-expand]').count())
            page.locator('[data-project-configure]').click()
            row=page.locator('[data-settings-agent="dsh-win"]')
            row.locator('[data-member-provider]').select_option('route-b')
            check('provider selection lists only its own model',row.locator('[data-member-model]').input_value()=='["route-b","model-two"]' and row.locator('[data-member-model] option').count()==2)
            row.locator('[data-member-effort]').select_option('low')
            page.locator('[data-settings-agent="codex-win"] [data-member-model]').select_option('gpt-6-luna')
            page.locator('[data-settings-agent="codex-win"] [data-member-effort]').select_option('low')
            page.locator('[data-settings-agent="hermes-win"] [data-member-effort]').select_option('high')
            page.locator('#project-member-settings button.primary').click()
            expect(page.locator('#modal')).not_to_be_visible()
            check('central dialog saves all project member choices',page.evaluate("__providerDemo.chats['project-chat-dsh-win'].sessions[0].model==='[\"route-b\",\"model-two\"]'"))
            page.fill('#message-input','Codex规划，DSH执行，Hermes验收')
            page.locator('#message-input').press('Enter')
            page.wait_for_function('__collaborationCalls===1')
            check('Enter starts one collaboration without plan confirmation',not page.locator('[data-confirm-project],#role-form').count())
            check('team page contains no workflow cards or reasoning',not page.locator('[data-workflow-id],.workflow-card,.thought').count())
            page.locator('[data-project-module="progress"]').click()
            expect(page.locator('[role="progressbar"]')).to_have_attribute('aria-valuenow','100')
            expect(page.locator('.project-member-status')).to_have_count(3)
            check('progress has member statuses and completion percentage')
            page.locator('[data-project-module="codex-win"]').click()
            expect(page.locator('[data-attempt-id="planner"] .agent-turn-text')).to_contain_text('这是一次普通讨论回复')
            check('agent page is one continuous transcript without model controls or bubbles',not page.locator('[data-project-chat-model],.bubble,.workflow-card,.project-agent-room-activity').count())
            check('planning protocol is readable and raw JSON starts collapsed',not page.locator('[data-attempt-id="planner"] .agent-protocol').evaluate('node=>node.open'))
            for width in [980,1440]:
                page.set_viewport_size({'width':width,'height':1000})
                check(str(width)+' width fits without horizontal overflow',page.evaluate('document.documentElement.scrollWidth<=innerWidth'))
            # Shared scrolling/Markdown regression coverage is kept from the prior UI suite.
            source=(ROOT/'scripts/verify_model_provider_ui.py').read_text(encoding='utf-8')
            start=source.index('transcript=page.evaluate(')
            end=source.index('            assert all(transcript.values())',start)
            scope={'page':page}
            exec(source[start:end].strip(),scope)
            transcript=scope['transcript']
            for name,result in transcript.items(): check('transcript '+name,result)
            page.locator('[data-project-module="team"]').click()
            page.evaluate("""() => {const d=__providerDemo.detail;d.workflows[0].status='failed';d.workflows[0].tasks=[{id:'task',position:0,title:'未完成任务',agent_id:'dsh-win',files:['calc.py'],depends_on:[],status:'queued'}];d.workflows[0].error='测试中断';}""")
            page.locator('[data-project-module="progress"]').click()
            page.locator('[data-continue-project]').click()
            page.wait_for_function('__continueCalls===1')
            check('progress page continues existing workflow without replanning')
            page.evaluate("__providerDemo.detail.workflows[0].status='interrupted'")
            page.locator('[data-project-module="team"]').click()
            page.locator('[data-project-module="progress"]').click()
            page.locator('[data-workflow-id]').click(button='right')
            page.locator('[data-message-delete]').click()
            page.wait_for_function('__removedItem?.kind==="workflow"')
            expect(page.locator('[data-workflow-id]')).to_have_count(0)
            check('workflow history remains deletable from progress page')
            check('no JavaScript errors',not errors)
            browser.close();success=True
    finally:
        server.terminate();server.wait(timeout=10)
        (ROOT/'artifacts/project-v17-ui.json').write_text(json.dumps({'success':success,'passed':len(checks),'checks':checks},ensure_ascii=False,indent=2),encoding='utf-8')

if __name__=='__main__': main()
