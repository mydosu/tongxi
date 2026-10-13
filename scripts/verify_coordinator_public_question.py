"""Real public-question UI, same-manager-session continuation, and adapter smoke.

Requires AGENT_HUB_LIVE_COORDINATOR=1. Only approved Luna/Daily Two low models
are selected; the app data and project live in disposable isolated directories.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import time
import uuid

from playwright.sync_api import expect, sync_playwright

import smoke_desktop as desktop
import verify_coordinator_live as live
import verify_stage6 as groups

ROOT = Path(__file__).resolve().parents[1]
EXE = Path(os.environ.get("AGENT_HUB_EXE", str(ROOT / "release" / "Agent Hub.exe"))).resolve()
REPORT = ROOT / "artifacts" / "coordinator-public-question-verification.json"
checks: list[str] = []
evidence: dict[str, object] = {}
failure_stage = "preflight"
failure_category: str | None = None
failure_check: str | None = None
last_workflow: dict | None = None


def check(label: str, condition: bool = True) -> None:
    if not condition:
        global failure_check
        failure_check = label
        raise AssertionError(label)
    checks.append(label)
    print("PASS " + label, flush=True)


def coordinator_sid(workflow_id: str) -> str | None:
    with sqlite3.connect(desktop.DATA / "hub.db") as db:
        row = db.execute("SELECT native_session_id FROM coordinator_sessions WHERE workflow_id=?", (workflow_id,)).fetchone()
    return row[0] if row else None


def wait_for_question(page, room: str, workflow_id: str, timeout: int = 900) -> tuple[dict, dict]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        detail = desktop.ipc(page, "get_conversation", id=room)
        workflow = next((item for item in detail.get("workflows", []) if item["id"] == workflow_id), None)
        if workflow is None:
            raise AssertionError("initiating workflow disappeared")
        question = next((item for item in detail.get("project_questions", []) if item.get("workflow_id") == workflow_id and item.get("status") == "pending"), None)
        if question:
            return workflow, question
        if workflow.get("status") in ("failed", "completed", "interrupted"):
            raise AssertionError("workflow stopped without a public question")
        page.wait_for_timeout(250)
    raise TimeoutError("manager did not publish a question before timeout")


def wait_completed(page, room: str, workflow_id: str, timeout: int = 900) -> dict:
    global last_workflow
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        detail = desktop.ipc(page, "get_conversation", id=room)
        workflow = next((item for item in detail.get("workflows", []) if item["id"] == workflow_id), None)
        if workflow:
            last_workflow = workflow
            # Use the same redacted actor/purpose/status progress format as the main harness.
            routes = (workflow.get("coordination") or {}).get("routes", [])
            purposes = {route.get("task_id"): route.get("purpose") for route in routes}
            summary = {
                "workflow_status": workflow.get("status"),
                "tasks": [{"agent_id": task.get("agent_id"), "purpose": purposes.get(task.get("id")), "status": task.get("status")} for task in workflow.get("tasks", [])],
                "attempts": [{"agent_id": attempt.get("agent_id"), "activity": attempt.get("activity"), "status": attempt.get("status")} for attempt in workflow.get("attempts", [])],
            }
            signature = json.dumps(summary, sort_keys=True)
            if signature != wait_completed.last_signature:
                print("PROGRESS " + json.dumps(summary, ensure_ascii=False), flush=True)
                wait_completed.last_signature = signature
            if workflow.get("status") == "completed":
                return workflow
            if workflow.get("status") in ("failed", "interrupted"):
                raise AssertionError("workflow did not resume after the submitted answer")
        page.wait_for_timeout(250)
    raise TimeoutError("same workflow did not complete after answer")


wait_completed.last_signature = None


def safe_exception(exc: BaseException) -> str:
    message = " ".join([str((live.failure_diagnostics(last_workflow) or {}).get("workflow_error") or ""), type(exc).__name__]).lower()
    if any(term in message for term in ("429", "402", "rate limit", "quota", "限额", "频率限制")):
        return "provider_limit"
    if isinstance(exc, TimeoutError):
        return "timeout"
    if isinstance(exc, AssertionError):
        return "acceptance_assertion"
    return type(exc).__name__


def run() -> None:
    global failure_stage, failure_category, failure_check, last_workflow
    if os.environ.get("AGENT_HUB_LIVE_COORDINATOR") != "1":
        raise SystemExit("Set AGENT_HUB_LIVE_COORDINATOR=1 to run real-model coordinator acceptance.")
    if EXE != (ROOT / "release" / "Agent Hub.exe").resolve() or not EXE.is_file():
        raise SystemExit("Use the packaged release/Agent Hub.exe for this acceptance.")

    success = False
    proc = None
    browser = None
    page = None
    project_root = Path(tempfile.mkdtemp(prefix="tongxi-coordinator-question-project-"))
    desktop.EXE = EXE
    desktop.DATA = Path(tempfile.gettempdir()) / ("tongxi-coordinator-question-data-" + uuid.uuid4().hex[:10])
    desktop.ISOLATED_SESSION_DB = True
    desktop.errors.clear()
    live.checks.clear()
    live.evidence.clear()
    live.workflow_timelines.clear()
    live.last_workflow = None
    live.failure_check = None
    REPORT.parent.mkdir(exist_ok=True)
    evidence["executable_sha256"] = hashlib.sha256(EXE.read_bytes()).hexdigest()
    evidence["project_root"] = str(project_root)
    evidence["data_directory"] = str(desktop.DATA)
    try:
        failure_stage = "fixture"
        (project_root / "calc.py").write_text("def square(value):\n    return value\n", encoding="utf-8")
        subprocess.run(["git", "init", "-q"], cwd=project_root, check=True, timeout=30)
        subprocess.run(["git", "config", "user.name", "Coordinator Question Test"], cwd=project_root, check=True, timeout=30)
        subprocess.run(["git", "config", "user.email", "coordinator-question@localhost"], cwd=project_root, check=True, timeout=30)
        subprocess.run(["git", "add", "calc.py"], cwd=project_root, check=True, timeout=30)
        subprocess.run(["git", "commit", "-q", "-m", "isolated public question fixture"], cwd=project_root, check=True, timeout=30)
        evidence["project_baseline"] = subprocess.run(["git", "rev-parse", "HEAD"], cwd=project_root, capture_output=True, text=True, check=True, timeout=30).stdout.strip()

        with sync_playwright() as playwright:
            failure_stage = "launch"
            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on("pageerror", lambda error: desktop.errors.append(str(error)))
            failure_stage = "approved_models"
            catalogs = {
                "codex-win": live.connected_models(page, "codex-win", "codex"),
                "dsh-win": live.connected_models(page, "dsh-win", "dsh"),
                "hermes-win": live.connected_models(page, "hermes-win", "hermes"),
            }
            models = {
                "codex-win": live.choose_model(catalogs["codex-win"], "luna"),
                "dsh-win": live.choose_model(catalogs["dsh-win"], "daily_two"),
                "hermes-win": live.choose_model(catalogs["hermes-win"], "daily_two"),
            }
            evidence["approved_models"] = {agent: {"id": model["id"], "provider_id": model.get("provider_id"), "provider_name": model.get("provider_name"), "effort": "low"} for agent, model in models.items()}

            failure_stage = "isolated_project"
            project = desktop.ipc(page, "register_project", name="管家公开提问验收", root=str(project_root))
            room = groups.group(page, "管家公开提问与继续验收", ["codex-win", "dsh-win", "hermes-win"])
            desktop.ipc(page, "bind_project", conversationId=room, projectId=project["id"])
            detail = desktop.ipc(page, "get_conversation", id=room)
            project_chats = {chat["agent_id"]: chat["conversation_id"] for chat in detail["project_agent_chats"]}
            for agent, model in models.items():
                desktop.ipc(page, "set_session_settings", id=project_chats[agent], agentId=agent, model=model["id"], reasoningEffort="low")
            # The project is bound through native IPC; reselect the public group
            # through the UI so its cached ConversationDetail refreshes as well.
            desktop.choose(page, room)
            expect(page.locator('[data-project-module="team"]')).to_be_visible(timeout=15000)

            failure_stage = "public_question"
            live.set_coordinator(page, room, "codex-win", models["codex-win"])
            request = (
                "请先在团队对话里公开问我一个带两个选项的问题，确认DSH应回复函数名还是函数用途；不得先派任务。"
                "我实际选择并提交后，请在同一工作流和管家会话中把任务交给DSH项目成员会话，只用一句话回答calc.py里函数名，不修改文件。"
                "所有成员沿用项目配置模型。"
            )
            workflow_id, started = live.start(page, room, request)
            check("one workflow starts for the user's original request", started.get("id") == workflow_id)
            workflow, question = wait_for_question(page, room, workflow_id)
            last_workflow = workflow
            check("manager publishes a public question with two options", question.get("agent_id") == "codex-win" and len(question.get("options", [])) == 2)
            check("question belongs to the same group and workflow", question.get("conversation_id") == room and question.get("workflow_id") == workflow_id)
            question_messages = [message for message in desktop.ipc(page, "get_conversation", id=room).get("project_team_messages", []) if message.get("id") == question["id"]]
            check("question has a public team-message entry", len(question_messages) == 1 and question_messages[0].get("kind") == "agent")
            question_button = page.locator(f'[data-answer-project-question="{question["id"]}"]')
            expect(question_button).to_be_visible(timeout=15000)
            check("team UI displays the public question without internal route JSON", question["prompt"] in page.locator(f'[data-team-message-id="{question["id"]}"]').inner_text() and '"action"' not in page.locator("#messages").inner_text())
            check("no manager-private page or member transcript is selected", page.locator('[data-project-module="team"].active').count() == 1 and page.locator('[data-project-module="codex-win"].active').count() == 0 and page.locator(".project-agent-room-activity").count() == 0)
            manager_sid_before = coordinator_sid(workflow_id)
            check("hidden coordinator SID exists before answer", bool(manager_sid_before))

            failure_stage = "answer_through_ui"
            choice = next((option for option in question["options"] if "函数名" in option), question["options"][0])
            question_button.click()
            expect(page.locator("#project-answer-form")).to_be_visible()
            option = page.locator("#project-answer-option")
            expect(option).to_be_visible()
            option.select_option(choice)
            expect(page.locator("#project-answer-text")).to_have_value(choice)
            page.locator("#project-answer-form button.primary").click()
            expect(page.locator("#project-answer-form")).not_to_be_visible(timeout=15000)
            answered_workflow = wait_completed(page, room, workflow_id)
            last_workflow = answered_workflow
            detail = desktop.ipc(page, "get_conversation", id=room)
            answered_question = next(item for item in detail.get("project_questions", []) if item.get("id") == question["id"])
            manager_sid_after = coordinator_sid(workflow_id)
            check("selected option was submitted as the question answer", answered_question.get("status") == "answered" and answered_question.get("answer") == choice)
            check("answer resumes the existing workflow without creating another", len(detail.get("workflows", [])) == 1 and detail["workflows"][0]["id"] == workflow_id)
            check("the same hidden manager SID continues after the answer", bool(manager_sid_before) and manager_sid_after == manager_sid_before)
            check("manager routes the resumed work to DSH as one reply task", len(answered_workflow.get("tasks", [])) == 1 and answered_workflow["tasks"][0].get("agent_id") == "dsh-win" and answered_workflow.get("coordination", {}).get("routes", [{}])[0].get("purpose") == "reply")
            check("DSH replies with the requested function name", bool(answered_workflow.get("tasks")) and "square" in answered_workflow["tasks"][0].get("output", "").lower())
            check("public answer does not create project files", [path.name for path in project_root.iterdir() if path.name != ".git"] == ["calc.py"])
            for attempt in answered_workflow.get("attempts", []):
                approved = models[attempt["agent_id"]]
                check(attempt["agent_id"] + " used its approved model at low effort", attempt.get("model") == approved["id"] and attempt.get("reasoning_effort") == "low" and "sol" not in approved["id"].lower())
                if attempt.get("activity") == "coordinate":
                    check("manager question/continuation made no native tool calls", not live.manager_tool_events(attempt))
            live.check_manager_and_worker_sessions(page, room, answered_workflow, "codex-win")
            evidence["public_question_workflow"] = live.workflow_evidence(answered_workflow, request)
            evidence["public_question"] = {
                "workflow_id": workflow_id,
                "question_agent": question.get("agent_id"),
                "public_team_message": True,
                "option_count": len(question.get("options", [])),
                "selected_option": choice,
                "status_after_submit": answered_question.get("status"),
                "same_workflow_count": len(detail.get("workflows", [])),
                "manager_sid_fingerprint_before": hashlib.sha256(manager_sid_before.encode()).hexdigest()[:16] if manager_sid_before else None,
                "manager_sid_fingerprint_after": hashlib.sha256(manager_sid_after.encode()).hexdigest()[:16] if manager_sid_after else None,
            }

            failure_stage = "remaining_manager_adapters"
            # Exercise the other two native manager adapters without replaying the main workflow.
            for manager, worker in (("hermes-win", "hermes-win"), ("dsh-win", "dsh-win")):
                job = live.run_manager_smoke(page, room, manager, worker, models, project_root)
                last_workflow = job
                for attempt in job.get("attempts", []):
                    approved = models[attempt["agent_id"]]
                    check(manager + " smoke used the approved low model", attempt.get("model") == approved["id"] and attempt.get("reasoning_effort") == "low")
                    if attempt.get("activity") == "coordinate":
                        check(manager + " manager made no native tool calls", not live.manager_tool_events(attempt))
                evidence["manager_smoke_" + manager.removesuffix("-win")] = live.evidence["manager_smoke_" + manager.removesuffix("-win")]

            check("window reported no JavaScript errors", not desktop.errors)
            success = True
    except BaseException as exc:
        failure_category = safe_exception(exc)
        if failure_check is None:
            failure_check = live.failure_check
        print("FAIL " + failure_stage + " (" + failure_category + ")", flush=True)
    finally:
        if page is not None:
            try:
                page.keyboard.press("Alt+F4")
                if proc is not None:
                    proc.wait(timeout=12)
            except Exception:
                pass
        if proc is not None and proc.poll() is None:
            try:
                subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"], capture_output=True, timeout=12)
            except Exception:
                proc.terminate()
                try:
                    proc.wait(timeout=10)
                except Exception:
                    pass
        if browser is not None:
            try:
                browser.close()
            except Exception:
                pass
        temp = Path(tempfile.gettempdir()).resolve()
        for target in (project_root, desktop.DATA):
            try:
                resolved = target.resolve()
                valid_prefix = resolved.name.startswith("tongxi-coordinator-question-project-") if target == project_root else resolved.name.startswith("tongxi-coordinator-question-data-")
                if resolved.parent == temp and valid_prefix and resolved.exists():
                    shutil.rmtree(resolved)
            except Exception:
                pass
        workflows = {
            value.get("workflow_id"): value.get("attempts", [])
            for value in evidence.values()
            if isinstance(value, dict)
            and isinstance(value.get("workflow_id"), str)
            and isinstance(value.get("attempts"), list)
        }
        if last_workflow is not None and isinstance(last_workflow.get("id"), str):
            attempts = last_workflow.get("attempts", [])
            if attempts or last_workflow["id"] not in workflows:
                workflows[last_workflow["id"]] = attempts
        attempts = sum(len(attempts) for attempts in workflows.values())
        report = {
            "success": success,
            "passed": len(checks) + len(live.checks),
            "checks": checks + live.checks,
            "failure_stage": None if success else failure_stage,
            "failure_category": failure_category,
            "failure_check": failure_check,
            "failure_diagnostics": live.failure_diagnostics(last_workflow or live.last_workflow) if not success else None,
            "native_turn_attempts": attempts,
            "native_turn_attempts_note": "Harness app-server turn attempts; provider-internal calls are not counted.",
            "model_requests": None,
            "model_requests_note": "Provider-internal model requests are not measured by this harness.",
            "provider_switches": 0,
            "sol_calls": 0,
            "sol_calls_note": "Direct harness selections use only approved Luna and Daily Two low models; provider-internal request counts are not measured.",
            "executable_sha256": evidence.get("executable_sha256"),
            "evidence": evidence,
        }
        REPORT.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps({"success": success, "passed": report["passed"], "failure_stage": report["failure_stage"], "failure_category": failure_category, "report": str(REPORT)}, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    run()
