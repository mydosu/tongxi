"""Isolated real-model smoke for the DSH coordinator adapter.

Set AGENT_HUB_LIVE_COORDINATOR=1 to authorize the single Luna/Daily Two low
model workflow. The app database and project fixture are disposable.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import uuid

from playwright.sync_api import sync_playwright

import smoke_desktop as desktop
import verify_coordinator_live as live
import verify_stage3 as connection_helpers
import verify_stage6 as groups

ROOT = Path(__file__).resolve().parents[1]
EXE = Path(os.environ.get("AGENT_HUB_EXE", str(ROOT / "release" / "Agent Hub.exe"))).resolve()
REPORT = ROOT / "artifacts" / "coordinator-dsh-adapter-verification.json"


def safe_workflow_summary(workflow: dict | None) -> dict | None:
    details = live.failure_diagnostics(workflow)
    if not details:
        return None
    route = details.get("last_manager_route_json")
    return {
        "workflow_id": details.get("workflow_id"),
        "workflow_status": details.get("workflow_status"),
        "workflow_error_present": bool(details.get("workflow_error")),
        "manager_route_action": route.get("action") if isinstance(route, dict) else None,
        "attempts": [
            {
                "agent_id": attempt.get("agent_id"),
                "activity": attempt.get("activity"),
                "stage": attempt.get("stage"),
                "status": attempt.get("status"),
                "model": attempt.get("model"),
                "reasoning_effort": attempt.get("reasoning_effort"),
                "native_thread_id_present": bool(attempt.get("native_thread_id")),
                "tool_event_count": attempt.get("tool_event_count"),
                "error_present": bool(attempt.get("error")),
            }
            for attempt in details.get("attempts", [])
        ],
        "tasks": [
            {
                "agent_id": task.get("agent_id"),
                "purpose": task.get("purpose"),
                "status": task.get("status"),
                "error_present": bool(task.get("error")),
            }
            for task in details.get("tasks", [])
        ],
    }


def main() -> None:
    if os.environ.get("AGENT_HUB_LIVE_COORDINATOR") != "1":
        raise SystemExit("Set AGENT_HUB_LIVE_COORDINATOR=1 to run the real-model adapter smoke.")
    if EXE != (ROOT / "release" / "Agent Hub.exe").resolve() or not EXE.is_file():
        raise SystemExit("Use the packaged release/Agent Hub.exe for this smoke.")

    proc = None
    browser = None
    page = None
    project_root = Path(tempfile.mkdtemp(prefix="tongxi-coordinator-dsh-project-")).resolve()
    desktop.EXE = EXE
    desktop.DATA = Path(tempfile.gettempdir()) / ("tongxi-coordinator-dsh-data-" + uuid.uuid4().hex[:10])
    desktop.ISOLATED_SESSION_DB = True
    desktop.errors.clear()
    live.checks.clear()
    live.evidence.clear()
    live.workflow_timelines.clear()
    live.last_workflow = None
    live.failure_check = None
    failure_stage = "fixture"
    failure_category = None
    last_workflow = None
    evidence: dict[str, object] = {
        "executable_sha256": hashlib.sha256(EXE.read_bytes()).hexdigest(),
        "project_root": str(project_root),
        "data_directory": str(desktop.DATA),
    }
    REPORT.parent.mkdir(exist_ok=True)
    success = False
    try:
        (project_root / "calc.py").write_text("def square(value):\n    return value\n", encoding="utf-8")
        subprocess.run(["git", "init", "-q"], cwd=project_root, check=True, timeout=30)
        subprocess.run(["git", "config", "user.name", "Coordinator DSH Test"], cwd=project_root, check=True, timeout=30)
        subprocess.run(["git", "config", "user.email", "coordinator-dsh@localhost"], cwd=project_root, check=True, timeout=30)
        subprocess.run(["git", "add", "calc.py"], cwd=project_root, check=True, timeout=30)
        subprocess.run(["git", "commit", "-q", "-m", "isolated DSH adapter fixture"], cwd=project_root, check=True, timeout=30)
        evidence["project_baseline"] = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=project_root, capture_output=True, text=True, check=True, timeout=30
        ).stdout.strip()

        with sync_playwright() as playwright:
            failure_stage = "launch"
            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            page.on("pageerror", lambda error: desktop.errors.append(str(error)))

            failure_stage = "approved_models"
            luna_catalog = live.connected_models(page, "codex-win", "codex")
            dsh_catalog = live.connected_models(page, "dsh-win", "dsh")
            models = {
                "codex-win": live.choose_model(luna_catalog, "luna"),
                "dsh-win": live.choose_model(dsh_catalog, "daily_two"),
            }
            evidence["approved_models"] = {
                agent: {
                    "id": model["id"],
                    "provider_id": model.get("provider_id"),
                    "provider_name": model.get("provider_name"),
                    "effort": "low",
                }
                for agent, model in models.items()
            }

            failure_stage = "isolated_project"
            project = desktop.ipc(page, "register_project", name="DSH coordinator adapter smoke", root=str(project_root))
            room = groups.group(page, "DSH coordinator adapter smoke", ["codex-win", "dsh-win"])
            desktop.ipc(page, "bind_project", conversationId=room, projectId=project["id"])
            detail = desktop.ipc(page, "get_conversation", id=room)
            project_chats = {chat["agent_id"]: chat["conversation_id"] for chat in detail["project_agent_chats"]}
            for agent, model in models.items():
                desktop.ipc(
                    page,
                    "set_session_settings",
                    id=project_chats[agent],
                    agentId=agent,
                    model=model["id"],
                    reasoningEffort="low",
                )
            desktop.choose(page, room)
            failure_stage = "dsh_manager_adapter"
            job = live.run_manager_smoke(page, room, "dsh-win", "dsh-win", models, project_root)
            last_workflow = job
            for attempt in job.get("attempts", []):
                model = models.get(attempt.get("agent_id"), {})
                live.check(
                    "DSH adapter uses the approved low model",
                    attempt.get("model") == model.get("id") and attempt.get("reasoning_effort") == "low",
                )
                if attempt.get("activity") == "coordinate":
                    live.check("DSH manager makes no native tool calls", not live.manager_tool_events(attempt))
            evidence["executable_sha256"] = hashlib.sha256(EXE.read_bytes()).hexdigest()
            evidence["manager_smoke_dsh"] = live.evidence["manager_smoke_dsh"]
            live.check("window reported no JavaScript errors", not desktop.errors)
            success = True
    except BaseException as exc:
        failure_category = live.safe_failure(exc)
        if live.failure_check is None:
            live.failure_check = str(exc) if isinstance(exc, AssertionError) else type(exc).__name__
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
        if success:
            temp = Path(tempfile.gettempdir()).resolve()
            for target, prefix in (
                (project_root, "tongxi-coordinator-dsh-project-"),
                (desktop.DATA, "tongxi-coordinator-dsh-data-"),
            ):
                try:
                    resolved = target.resolve()
                    if resolved.parent == temp and resolved.name.startswith(prefix) and resolved.exists():
                        shutil.rmtree(resolved)
                except Exception:
                    pass

        workflows: dict[str, list] = {}
        for value in evidence.values():
            if isinstance(value, dict) and isinstance(value.get("workflow_id"), str) and isinstance(value.get("attempts"), list):
                if value["attempts"] or value["workflow_id"] not in workflows:
                    workflows[value["workflow_id"]] = value["attempts"]
        observed_workflow = last_workflow or live.last_workflow
        if observed_workflow is not None and isinstance(observed_workflow.get("id"), str):
            attempts = observed_workflow.get("attempts", [])
            if attempts or observed_workflow["id"] not in workflows:
                workflows[observed_workflow["id"]] = attempts
        failure_summary = safe_workflow_summary(observed_workflow) if not success else None
        if failure_summary:
            evidence["failed_workflow_summary"] = failure_summary
        report = {
            "success": success,
            "passed": len(live.checks),
            "checks": live.checks,
            "failure_stage": None if success else failure_stage,
            "failure_category": failure_category,
            "failure_check": live.failure_check,
            "failure_diagnostics": failure_summary,
            "native_turn_attempts": sum(len(items) for items in workflows.values()),
            "native_turn_attempts_note": "Harness app-server turn attempts; provider-internal calls are not counted.",
            "model_requests": None,
            "model_requests_note": "Provider-internal model requests are not measured by this harness.",
            "sol_calls": 0,
            "sol_calls_note": "Direct harness selections use only approved Luna and Daily Two low models; provider-internal request counts are not measured.",
            "executable_sha256": evidence.get("executable_sha256"),
            "evidence": evidence,
        }
        REPORT.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps({"success": success, "passed": report["passed"], "failure_stage": report["failure_stage"], "failure_category": failure_category, "report": str(REPORT)}, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
