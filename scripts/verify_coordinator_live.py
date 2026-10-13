"""Real coordinator end-to-end acceptance on a disposable project.

This script uses only GPT-6 Luna and DeepSeek V4.1 Flash through Daily Two. It
does not retry or switch providers after a quota/rate-limit failure, never reads
or prints credentials, and terminates the app process tree it launched.

To prevent accidental paid calls, set AGENT_HUB_LIVE_COORDINATOR=1 explicitly.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import tempfile
import time
import uuid

from playwright.sync_api import sync_playwright

import smoke_desktop as desktop
import verify_stage3 as connection_helpers
import verify_stage6 as groups

ROOT = Path(__file__).resolve().parents[1]
EXE = Path(os.environ.get("AGENT_HUB_EXE", str(ROOT / "release" / "Agent Hub.exe")))
ARTIFACTS = ROOT / "artifacts"
REPORT = ARTIFACTS / "coordinator-live-verification.json"
checks: list[str] = []
evidence: dict[str, object] = {}
stage = "preflight"
failure_category: str | None = None
failure_check: str | None = None
failure_details: dict[str, object] | None = None
last_workflow: dict | None = None
solution_branch_checks: list[dict[str, object]] = []
checked_solution_tasks: set[str] = set()
workflow_timelines: dict[str, dict[str, object]] = {}


def check(label: str, condition: bool = True, details: dict[str, object] | None = None) -> None:
    if not condition:
        global failure_check, failure_details
        failure_check = label
        failure_details = details
        raise AssertionError(label)
    checks.append(label)
    print("PASS " + label, flush=True)


def manager_tool_events(attempt: dict) -> list[dict]:
    # app-server forwards this lifecycle metadata in the same collection as
    # tools, but it is not a tool invocation.
    return [event for event in attempt.get("tool_events", []) if event.get("kind") != "subAgentActivity"]


def connected_models(page, agent: str, command: str) -> list[dict]:
    status = desktop.ipc(page, command + "_status")
    if status.get("connection") != "connected":
        desktop.ipc(page, "connect_" + command)
        connection_helpers.wait_connection(page, command)
        status = desktop.ipc(page, command + "_status")
    if status.get("connection") != "connected":
        raise AssertionError("member connection unavailable: " + agent)
    return status["models"]


def choose_model(models: list[dict], kind: str) -> dict:
    if kind == "luna":
        matches = [model for model in models if "gpt-6-luna" in (model.get("id", "") + " " + model.get("name", "")).lower()]
    else:
        matches = [
            model for model in models
            if "v4.1" in model.get("id", "").lower()
            and (model.get("provider_id") == "command-code-daily-2" or "daily two" in model.get("provider_name", "").lower())
        ]
    if not matches:
        raise AssertionError("required approved model is not available: " + kind)
    model = matches[0]
    if "low" not in model.get("efforts", []):
        raise AssertionError("approved model does not expose low reasoning effort")
    return model


def wait_workflow(page, room: str, workflow_id: str, timeout: int = 1800) -> dict:
    global last_workflow
    deadline = time.monotonic() + timeout
    previous_progress = None
    timeline: dict[str, object] = {"observations": [], "tasks": {}, "last_signature": None}
    workflow_timelines[workflow_id] = timeline
    while time.monotonic() < deadline:
        detail = desktop.ipc(page, "get_conversation", id=room)
        job = next((item for item in detail.get("workflows", []) if item["id"] == workflow_id), None)
        if job is not None:
            last_workflow = job
            routes = (job.get("coordination") or {}).get("routes", [])
            purpose_by_task = {route.get("task_id"): route.get("purpose") for route in routes}
            observed_at = time.time()
            observations = timeline["observations"]
            task_history = timeline["tasks"]
            signature = (job.get("status"), tuple(sorted((item.get("id"), item.get("status")) for item in job.get("tasks", []) if item.get("id"))))
            if signature != timeline["last_signature"]:
                observations.append({"observed_at": observed_at, "workflow_status": job.get("status"), "tasks": [{"task_id": item.get("id"), "status": item.get("status"), "purpose": purpose_by_task.get(item.get("id"))} for item in job.get("tasks", [])]})
                timeline["last_signature"] = signature
            for item in job.get("tasks", []):
                task_id = item.get("id")
                if not isinstance(task_id, str):
                    continue
                history = task_history.setdefault(task_id, {"first_seen_at": observed_at, "first_completed_observed_at": None, "purpose": purpose_by_task.get(task_id), "agent_id": item.get("agent_id"), "depends_on": list(item.get("depends_on") or [])})
                if item.get("status") == "completed" and history["first_completed_observed_at"] is None:
                    history["first_completed_observed_at"] = observed_at
                history["last_observed_status"] = item.get("status")
            attempts = [
                {
                    "agent_id": item.get("agent_id"),
                    "activity": item.get("activity"),
                    "stage": item.get("stage"),
                    "status": item.get("status"),
                }
                for item in job.get("attempts", [])
            ]
            tasks = [
                {
                    "agent_id": item.get("agent_id"),
                    "purpose": purpose_by_task.get(item.get("id")),
                    "status": item.get("status"),
                }
                for item in job.get("tasks", [])
            ]
            progress = (job.get("status"), json.dumps(attempts, sort_keys=True), json.dumps(tasks, sort_keys=True))
            if progress != previous_progress:
                print(
                    "PROGRESS "
                    + json.dumps(
                        {"workflow_status": job.get("status"), "attempts": attempts, "tasks": tasks},
                        ensure_ascii=False,
                    ),
                    flush=True,
                )
                previous_progress = progress
                root = Path(evidence.get("project_root", ""))
                baseline = evidence.get("project_baseline")
                for task in job.get("tasks", []):
                    task_id = task.get("id")
                    if (
                        purpose_by_task.get(task_id) == "solution"
                        and task.get("status") == "completed"
                        and isinstance(task_id, str)
                        and task_id not in checked_solution_tasks
                        and root.is_dir()
                        and isinstance(baseline, str)
                    ):
                        checked_solution_tasks.add(task_id)
                        diff = subprocess.run(
                            ["git", "diff", "--name-only", baseline, "hub/" + task_id],
                            cwd=root,
                            capture_output=True,
                            text=True,
                            timeout=20,
                        )
                        changed_paths = [line for line in diff.stdout.splitlines() if line.strip()]
                        solution_branch_checks.append(
                            {
                                "task_id": task_id,
                                "agent_id": task.get("agent_id"),
                                "status": task.get("status"),
                                "git_exit_code": diff.returncode,
                                "changed_path_count": len(changed_paths),
                                "name_only_diff_empty": diff.returncode == 0 and not changed_paths,
                            }
                        )
                        evidence["solution_branch_diffs"] = list(solution_branch_checks)
        if job and job["status"] == "completed":
            return job
        if job and job["status"] in ("failed", "interrupted"):
            raise AssertionError("workflow stopped before completion")
        page.wait_for_timeout(250)
    raise TimeoutError("workflow did not complete before timeout")


def start(page, room: str, request: str) -> tuple[str, dict]:
    message_id = str(uuid.uuid4())
    job = desktop.ipc(
        page,
        "start_project",
        conversationId=room,
        messageId=message_id,
        content=request,
    )
    return job["id"], job


def set_coordinator(page, room: str, agent: str, model: dict) -> None:
    desktop.ipc(
        page,
        "set_project_coordinator",
        conversationId=room,
        coordinator={"agent": agent, "model": model["id"], "effort": "low"},
    )


def session_by_agent(page, group_id: str) -> dict[str, str | None]:
    detail = desktop.ipc(page, "get_conversation", id=group_id)
    sessions: dict[str, str | None] = {}
    for chat in detail.get("project_agent_chats", []):
        member_detail = desktop.ipc(page, "get_conversation", id=chat["conversation_id"])
        session = next((row for row in member_detail.get("sessions", []) if row["agent_id"] == chat["agent_id"]), None)
        sessions[chat["agent_id"]] = session.get("native_session_id") if session else None
    return sessions


def check_manager_and_worker_sessions(page, room: str, workflow: dict, manager: str) -> None:
    attempts = workflow.get("attempts", [])
    manager_attempts = [attempt for attempt in attempts if attempt.get("activity") == "coordinate"]
    worker_attempts = [attempt for attempt in attempts if attempt.get("task_id") and attempt.get("status") == "completed"]
    check(manager + " coordinator made a real native turn", bool(manager_attempts) and all(attempt.get("native_thread_id") for attempt in manager_attempts))
    with sqlite3.connect(desktop.DATA / "hub.db") as db:
        row = db.execute(
            "SELECT agent_id,native_session_id FROM coordinator_sessions WHERE workflow_id=?",
            (workflow["id"],),
        ).fetchone()
    check(manager + " coordinator SID is persisted on its workflow", bool(row) and row[0] == manager)
    check(manager + " persisted SID matches its coordinator turn", bool(row) and all(attempt.get("native_thread_id") == row[1] for attempt in manager_attempts))

    worker_sessions = session_by_agent(page, room)
    for agent in sorted({attempt["agent_id"] for attempt in worker_attempts}):
        own = [attempt for attempt in worker_attempts if attempt["agent_id"] == agent]
        worker_sid = worker_sessions.get(agent)
        check(agent + " project worker uses its project private-chat session", bool(worker_sid) and all(attempt.get("native_thread_id") == worker_sid for attempt in own))
        if agent == manager:
            check(manager + " coordinator and worker session IDs are separate", bool(row) and bool(worker_sid) and row[1] != worker_sid)


def session_isolation_evidence(page, room: str, workflow: dict, manager: str) -> dict:
    """Collect session isolation facts without writing raw native session IDs."""
    attempts = workflow.get("attempts", [])
    manager_attempts = [attempt for attempt in attempts if attempt.get("activity") == "coordinate"]
    worker_attempts = [attempt for attempt in attempts if attempt.get("task_id") and attempt.get("status") == "completed"]
    with sqlite3.connect(desktop.DATA / "hub.db") as db:
        row = db.execute("SELECT agent_id,native_session_id FROM coordinator_sessions WHERE workflow_id=?", (workflow["id"],)).fetchone()
    worker_sessions = session_by_agent(page, room)
    return {
        "manager_native_turns_have_thread_id": bool(manager_attempts) and all(attempt.get("native_thread_id") for attempt in manager_attempts),
        "manager_sid_persisted_for_workflow": bool(row) and row[0] == manager and bool(row[1]),
        "manager_turns_match_persisted_sid": bool(row) and all(attempt.get("native_thread_id") == row[1] for attempt in manager_attempts),
        "workers": {
            agent: {
                "worker_private_sid_present": bool(worker_sessions.get(agent)),
                "completed_turns_match_worker_sid": bool(worker_sessions.get(agent)) and all(attempt.get("native_thread_id") == worker_sessions.get(agent) for attempt in worker_attempts if attempt.get("agent_id") == agent),
                "separate_from_manager_sid": (row[1] != worker_sessions.get(agent)) if row and agent == manager else None,
                "sid_fingerprint": hashlib.sha256(worker_sessions[agent].encode()).hexdigest()[:16] if worker_sessions.get(agent) else None,
            }
            for agent in sorted({attempt["agent_id"] for attempt in worker_attempts})
        },
        "manager_sid_fingerprint": hashlib.sha256(row[1].encode()).hexdigest()[:16] if row and row[1] else None,
    }


def collect_prevalidation_evidence(page, room: str, workflow: dict, request: str, root: Path, models: dict[str, dict], original_check_hash: str) -> None:
    """Capture fixture, merge, verification, and session facts before assertions can stop validation."""
    baseline = evidence.get("project_baseline")
    files = {}
    for name in ("calc.py", "check.py", "classify.py", "CLOSEOUT.md"):
        path = root / name
        if path.is_file():
            payload = path.read_bytes()
            files[name] = {"present": True, "size_bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest()}
        else:
            files[name] = {"present": False, "size_bytes": 0, "sha256": None}
    git_commands = {
        "head": ["git", "rev-parse", "HEAD"],
        "status": ["git", "status", "--porcelain"],
        "changed_tracked": ["git", "diff", "--name-only", str(baseline)],
        "untracked": ["git", "ls-files", "--others", "--exclude-standard"],
        "merge_commits": ["git", "rev-list", "--count", "--merges", "HEAD", "^" + str(baseline)],
    }
    git = {}
    for key, command in git_commands.items():
        try:
            result = subprocess.run(command, cwd=root, capture_output=True, text=True, timeout=20)
            git[key] = {"returncode": result.returncode, "value": result.stdout.strip()}
        except Exception as exc:
            git[key] = {"returncode": None, "error_type": type(exc).__name__}
    check_path = root / "check.py"
    actual_hash = hashlib.sha256(check_path.read_bytes()).hexdigest() if check_path.is_file() else None
    try:
        test = subprocess.run(["python", "-B", "check.py"], cwd=root, capture_output=True, text=True, timeout=40)
        test_evidence = {"returncode": test.returncode, "stdout": safe_excerpt(test.stdout, 1000), "stderr": safe_excerpt(test.stderr, 1000)}
    except Exception as exc:
        test_evidence = {"returncode": None, "error_type": type(exc).__name__}
    try:
        sessions = session_isolation_evidence(page, room, workflow, "codex-win")
    except Exception as exc:
        sessions = {"collection_error_type": type(exc).__name__}
    route_by_task = {route.get("task_id"): route for route in workflow.get("coordination", {}).get("routes", [])}
    grouped: dict[str, list[dict]] = {}
    for task in workflow.get("tasks", []):
        route = route_by_task.get(task.get("id"), {})
        grouped.setdefault(route.get("purpose", "unrouted"), []).append(task)
    timeline = workflow_timelines.get(workflow.get("id"), {})
    solutions = grouped.get("solution", [])
    implementations = grouped.get("implementation", [])
    verification = grouped.get("verification", [])
    closeout = grouped.get("closeout", [])
    ordering_checks = {
        "solution_before_implementation": bool(solutions) and bool(implementations) and all(any(task_precedes(solution, implementation, timeline, "solution") for solution in solutions) for implementation in implementations),
        "implementation_before_verification": bool(implementations) and bool(verification) and all(task_precedes(implementation, verify, timeline, "implementation") for verify in verification for implementation in implementations),
        "implementation_and_verification_before_closeout": bool(implementations) and bool(verification) and bool(closeout) and all(
            task_precedes(predecessor, final, timeline, predecessor_purpose)
            for final in closeout
            for predecessor, predecessor_purpose in [
                *((task, "implementation") for task in implementations),
                *((task, "verification") for task in verification),
            ]
        ),
        "method": "stored dependency edge or completion observed before successor task first appeared",
    }
    attempt_policy_checks = []
    for attempt in workflow.get("attempts", []):
        agent = attempt.get("agent_id")
        expected = models.get(agent, {})
        model_ok = attempt.get("model") == expected.get("id") and attempt.get("reasoning_effort") == "low" and "sol" not in str(expected.get("id", "")).lower()
        manager_no_tools = attempt.get("activity") != "coordinate" or not manager_tool_events(attempt)
        attempt_policy_checks.append({
            "agent_id": agent,
            "activity": attempt.get("activity"),
            "task_id": attempt.get("task_id"),
            "selected_model_matches_approved_low": model_ok,
            "manager_has_no_tool_calls": manager_no_tools,
            "tool_event_count": len(manager_tool_events(attempt)) if attempt.get("activity") == "coordinate" else len(attempt.get("tool_events", [])),
            "event_record_count": len(attempt.get("tool_events", [])),
        })
    evidence["main_prevalidation"] = {
        "project_baseline": baseline,
        "final_files": files,
        "git": git,
        "check_py_sha256": actual_hash,
        "check_py_sha256_matches_original": actual_hash == original_check_hash,
        "check_py_execution": test_evidence,
        "session_isolation": sessions,
        "workflow_timeline": timeline,
        "ordering_checks": ordering_checks,
        "attempt_policy_checks": attempt_policy_checks,
        "fixture_is_controlled": True,
    }


def task_precedes(predecessor: dict, successor: dict, timeline: dict, required_purpose: str) -> bool:
    """Accept a dependency edge or observed completion before successor creation."""
    if predecessor.get("position") in set(successor.get("depends_on") or []):
        return True
    histories = timeline.get("tasks", {})
    before = histories.get(predecessor.get("id"), {})
    after = histories.get(successor.get("id"), {})
    completed_at = before.get("first_completed_observed_at")
    created_at = after.get("first_seen_at")
    return before.get("purpose") == required_purpose and completed_at is not None and created_at is not None and completed_at < created_at


def workflow_record(workflow: dict) -> dict:
    routes = workflow.get("coordination", {}).get("routes", [])
    tasks = workflow.get("tasks", [])
    by_id = {task["id"]: task for task in tasks}
    return {
        "workflow_id": workflow["id"],
        "status": workflow["status"],
        "routes": [
            {"task_id": route.get("task_id"), "purpose": route.get("purpose"), "agent_id": by_id.get(route.get("task_id"), {}).get("agent_id"), "source_id": route.get("source_id"), "quote": route.get("quote"), "context_ids": route.get("context_ids", [])}
            for route in routes
        ],
        "tasks": [{"id": task["id"], "purpose": next((route["purpose"] for route in routes if route["task_id"] == task["id"]), None), "agent_id": task["agent_id"], "status": task["status"], "depends_on": task["depends_on"], "model": task.get("model")} for task in tasks],
        "attempts": [{
            "agent_id": attempt["agent_id"], "activity": attempt.get("activity"), "stage": attempt["stage"], "status": attempt["status"],
            "task_id": attempt.get("task_id"), "model": attempt.get("model"), "reasoning_effort": attempt.get("reasoning_effort"),
            "native_thread_id": attempt.get("native_thread_id"), "tool_event_count": len(manager_tool_events(attempt)) if attempt.get("activity") == "coordinate" else len(attempt.get("tool_events", [])),
            "event_record_count": len(attempt.get("tool_events", [])),
            "tool_events": [{
                "kind": safe_excerpt(event.get("kind"), 80),
                "title": safe_excerpt(event.get("title"), 120),
                "status": safe_excerpt(event.get("status"), 80),
                "sha256": hashlib.sha256(json.dumps(event, ensure_ascii=False, sort_keys=True, default=str).encode("utf-8")).hexdigest(),
            } for event in attempt.get("tool_events", []) if isinstance(event, dict)],
        } for attempt in workflow.get("attempts", [])],
    }


def workflow_evidence(workflow: dict, request: str) -> dict:
    routes = workflow.get("coordination", {}).get("routes", [])
    tasks = workflow.get("tasks", [])
    check("every stored route uses an exact quote from the initiating user message", bool(routes) and all(route.get("source_id") == workflow["user_message_id"] and route.get("quote") and route["quote"] in request for route in routes))
    check("each task has a persisted route with purpose and original quote", len(routes) == len(tasks) and all(route.get("task_id") in {task.get("id") for task in tasks} and route.get("purpose") for route in routes))
    return workflow_record(workflow)


def safe_excerpt(text: object, limit: int = 360) -> str | None:
    if not isinstance(text, str) or not text.strip():
        return None
    excerpt = text.strip()[:limit]
    excerpt = re.sub(r"(?i)(api[_ -]?key|access[_ -]?token|authorization)\s*[:=]\s*[^\s,;]+", r"\1=[REDACTED]", excerpt)
    excerpt = re.sub(r"(?i)\b(?:sk|rk|ghp|gho|github_pat|Bearer)-[A-Za-z0-9_\-]{10,}\b", "[REDACTED]", excerpt)
    return excerpt


def safe_manager_json(attempts: list[dict]) -> dict | None:
    manager = next((attempt for attempt in reversed(attempts) if attempt.get("activity") == "coordinate"), None)
    if manager is None:
        return None
    output = manager.get("output", "")
    allowed = {"action", "routes", "withdraw", "question", "result"}
    if not isinstance(output, str) or len(output) > 64000:
        return {"valid_json": False, "characters": len(output) if isinstance(output, str) else 0}
    decoder = json.JSONDecoder()
    parsed = None
    for start in [match.start() for match in re.finditer(r"\{", output)]:
        try:
            candidate, _ = decoder.raw_decode(output[start:])
        except json.JSONDecodeError:
            continue
        if isinstance(candidate, dict) and set(candidate).issubset(allowed):
            parsed = candidate
            break
    if parsed is None:
        return {"valid_json": False, "characters": len(output)}

    route_fields = {"key", "task_id", "purpose", "agent_id", "source_id", "quote", "context_ids", "depends_on", "model", "effort", "model_source"}
    def safe_object(value: object) -> object:
        if isinstance(value, dict):
            return {key: safe_object(item) for key, item in value.items() if key not in {"instructions", "command", "api_key", "token", "headers"}}
        if isinstance(value, list):
            return [safe_object(item) for item in value[:32]]
        if isinstance(value, str):
            return safe_excerpt(value, 1200)
        if value is None or isinstance(value, (bool, int, float)):
            return value
        return None

    routes = parsed.get("routes", [])
    if not isinstance(routes, list) or any(not isinstance(route, dict) or not set(route).issubset(route_fields) for route in routes):
        return {"valid_json": False, "characters": len(output)}
    reference_fields = {"source_id", "quote"}
    if any(not isinstance(route.get("model_source"), dict) or not set(route["model_source"]).issubset(reference_fields) for route in routes if route.get("model_source") is not None):
        return {"valid_json": False, "characters": len(output)}
    withdrawals = parsed.get("withdraw", [])
    if isinstance(withdrawals, list) and any(not isinstance(item, dict) or not set(item).issubset({"task_id", "source_id", "quote"}) for item in withdrawals):
        return {"valid_json": False, "characters": len(output)}
    if parsed.get("result") is not None and (not isinstance(parsed["result"], dict) or not set(parsed["result"]).issubset(reference_fields)):
        return {"valid_json": False, "characters": len(output)}
    if parsed.get("question") is not None and (not isinstance(parsed["question"], dict) or not set(parsed["question"]).issubset({"prompt", "options"})):
        return {"valid_json": False, "characters": len(output)}
    return safe_object(parsed)  # route schema has no freeform instruction field


def failure_diagnostics(workflow: dict | None) -> dict | None:
    if workflow is None:
        return None
    attempts = workflow.get("attempts", [])
    tasks = workflow.get("tasks", [])
    coordination = workflow.get("coordination") or {}
    return {
        "workflow_id": workflow.get("id"),
        "workflow_status": workflow.get("status"),
        "workflow_error": safe_excerpt(workflow.get("error"), 600),
        "last_manager_route_json": safe_manager_json(attempts),
        "attempts": [{
            "agent_id": attempt.get("agent_id"), "activity": attempt.get("activity"), "stage": attempt.get("stage"),
            "status": attempt.get("status"), "task_id": attempt.get("task_id"), "model": attempt.get("model"),
            "reasoning_effort": attempt.get("reasoning_effort"), "native_thread_id": attempt.get("native_thread_id"),
            "tool_event_count": len(attempt.get("tool_events", [])),
            "error": safe_excerpt(attempt.get("error"), 600),
        } for attempt in attempts],
        "tasks": [{
            "task_id": task.get("id"), "agent_id": task.get("agent_id"), "status": task.get("status"),
            "purpose": next((route.get("purpose") for route in coordination.get("routes", []) if route.get("task_id") == task.get("id")), None),
            "output_summary": safe_excerpt(task.get("output"), 500), "error": safe_excerpt(task.get("error"), 600),
        } for task in tasks],
    }


def validate_main_workflow(page, room: str, workflow: dict, request: str, root: Path, models: dict[str, dict], original_check_hash: str) -> None:
    global last_workflow
    last_workflow = workflow
    evidence["main_workflow"] = workflow_record(workflow)
    collect_prevalidation_evidence(page, room, workflow, request, root, models, original_check_hash)
    tasks = workflow["tasks"]
    routes = workflow["coordination"]["routes"]
    routes_by_task = {route["task_id"]: route for route in routes}
    tasks_by_purpose: dict[str, list[dict]] = {}
    for task in tasks:
        tasks_by_purpose.setdefault(routes_by_task[task["id"]]["purpose"], []).append(task)
    check("Codex received the requested solution task", any(task["agent_id"] == "codex-win" for task in tasks_by_purpose.get("solution", [])))
    solution_tasks = tasks_by_purpose.get("solution", [])
    solution_diffs = [
        subprocess.run(
            ["git", "diff", "--name-only", evidence["project_baseline"], "hub/" + task["id"]],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=20,
        )
        for task in solution_tasks
    ]
    check(
        "solution task branches made no file changes before implementation handoff",
        bool(solution_tasks) and all(result.returncode == 0 and not result.stdout.strip() for result in solution_diffs),
    )
    check("Codex received its requested implementation slice", any(task["agent_id"] == "codex-win" and "calc.py" in routes_by_task[task["id"]]["quote"] for task in tasks_by_purpose.get("implementation", [])))
    check("DSH received the remaining implementation", any(task["agent_id"] == "dsh-win" and "classify.py" in routes_by_task[task["id"]]["quote"] for task in tasks_by_purpose.get("implementation", [])))
    check("DSH received the requested verification task", any(task["agent_id"] == "dsh-win" for task in tasks_by_purpose.get("verification", [])))
    check("Hermes received the requested closeout task", any(task["agent_id"] == "hermes-win" for task in tasks_by_purpose.get("closeout", [])))

    order = evidence["main_prevalidation"]["ordering_checks"]
    check("implementations wait for Codex's requested solution", order["solution_before_implementation"])
    check("verification waits for every implementation", order["implementation_before_verification"])
    check("closeout runs after all implementation and verification tasks", order["implementation_and_verification_before_closeout"])
    check("all routed tasks completed", all(task["status"] == "completed" for task in tasks))

    for attempt in workflow["attempts"]:
        expected = models[attempt["agent_id"]]
        check(attempt["agent_id"] + " used only its approved test model and low effort", attempt.get("model") == expected["id"] and attempt.get("reasoning_effort") == "low" and "sol" not in expected["id"].lower(), {
            "expected_model": expected.get("id"), "observed_model": attempt.get("model"),
            "expected_effort": "low", "observed_effort": attempt.get("reasoning_effort"),
        })
        if attempt.get("activity") == "coordinate":
            check("manager turn produced no tool calls", not manager_tool_events(attempt), {
                "observed_tool_event_count": len(manager_tool_events(attempt)),
                "metadata_event_kinds": [event.get("kind") for event in attempt.get("tool_events", []) if event.get("kind") == "subAgentActivity"],
            })

    check_manager_and_worker_sessions(page, room, workflow, "codex-win")

    check("DSH created the requested new module", (root / "classify.py").is_file())
    check("Hermes created the requested new closeout file", (root / "CLOSEOUT.md").is_file())
    check("the verification script was not modified", hashlib.sha256((root / "check.py").read_bytes()).hexdigest() == original_check_hash)
    test = subprocess.run(["python", "-B", "check.py"], cwd=root, capture_output=True, timeout=40)
    check("independent fixture assertions pass after merge", test.returncode == 0)
    check("closeout records the verification command", "python -B check.py" in (root / "CLOSEOUT.md").read_text(encoding="utf-8"))
    tracked = subprocess.run(["git", "diff", "--name-only", evidence["project_baseline"]], cwd=root, capture_output=True, text=True, timeout=20)
    untracked = subprocess.run(["git", "ls-files", "--others", "--exclude-standard"], cwd=root, capture_output=True, text=True, timeout=20)
    changed_paths = {path.replace("\\", "/") for path in tracked.stdout.splitlines()} | {path.replace("\\", "/") for path in untracked.stdout.splitlines()}
    check("only the three explicitly authorized project files changed", tracked.returncode == 0 and untracked.returncode == 0 and changed_paths == {"calc.py", "classify.py", "CLOSEOUT.md"})

    evidence["main_workflow"] = workflow_evidence(workflow, request)


def run_manager_smoke(page, room: str, manager: str, worker: str, models: dict[str, dict], test_project: Path) -> dict:
    global last_workflow
    set_coordinator(page, room, manager, models[manager])
    labels = {"hermes-win": "Hermes", "dsh-win": "DSH"}
    request = f"请{labels[manager]}按原话交给{labels[worker]}项目成员会话：只用一句话说明calc.py里的函数名，不要修改任何文件。"
    before = {path.relative_to(test_project).as_posix() for path in test_project.rglob("*") if path.is_file() and ".git" not in path.parts}
    workflow_id, _ = start(page, room, request)
    job = wait_workflow(page, room, workflow_id, timeout=900)
    last_workflow = job
    routes = job.get("coordination", {}).get("routes", [])
    tasks = job.get("tasks", [])
    check(manager + " independent manager adapter started", any(attempt.get("activity") == "coordinate" and attempt.get("status") == "completed" for attempt in job.get("attempts", [])))
    check(manager + " manager route reached the selected worker", len(tasks) == 1 and tasks[0]["agent_id"] == worker and routes and routes[0]["purpose"] == "reply")
    check(manager + " manager quote stays inside the user's exact request", len(routes) == 1 and routes[0]["quote"] in request and routes[0]["source_id"] == job["user_message_id"])
    check(manager + " selected worker replied with the requested project fact", bool(tasks) and "square" in tasks[0].get("output", "").lower())
    check_manager_and_worker_sessions(page, room, job, manager)
    after = {path.relative_to(test_project).as_posix() for path in test_project.rglob("*") if path.is_file() and ".git" not in path.parts}
    check(manager + " short reply round added no project files", before == after)
    evidence["manager_smoke_" + manager.removesuffix("-win")] = workflow_evidence(job, request)
    return job


def safe_failure(exc: BaseException) -> str:
    details = failure_diagnostics(last_workflow) or {}
    error_text = " ".join(
        [str(details.get("workflow_error") or "")]
        + [str(attempt.get("error") or "") for attempt in details.get("attempts", [])]
        + [str(task.get("error") or "") for task in details.get("tasks", [])]
    ).lower()
    message = error_text + " " + type(exc).__name__.lower()
    if any(term in message for term in ("429", "402", "rate limit", "quota", "限额", "频率限制")):
        return "provider_limit"
    if isinstance(exc, TimeoutError):
        return "timeout"
    if isinstance(exc, AssertionError):
        return "acceptance_assertion"
    return type(exc).__name__


def run() -> None:
    global stage, failure_category, failure_check, failure_details
    if os.environ.get("AGENT_HUB_LIVE_COORDINATOR") != "1":
        raise SystemExit("Set AGENT_HUB_LIVE_COORDINATOR=1 only when you intend to run isolated real-model acceptance.")
    if not EXE.is_file():
        raise SystemExit("The requested Agent Hub executable is missing.")
    if EXE.resolve() != (ROOT / "release" / "Agent Hub.exe").resolve():
        raise SystemExit("Real coordinator acceptance must use release/Agent Hub.exe.")

    success = False
    proc = None
    browser = None
    page = None
    test_root = Path(tempfile.mkdtemp(prefix="tongxi-coordinator-project-"))
    desktop.EXE = EXE
    desktop.DATA = Path(tempfile.gettempdir()) / ("tongxi-coordinator-data-" + uuid.uuid4().hex[:10])
    desktop.ISOLATED_SESSION_DB = True
    desktop.errors.clear()
    ARTIFACTS.mkdir(exist_ok=True)
    evidence["executable"] = str(EXE)
    evidence["executable_sha256"] = hashlib.sha256(EXE.read_bytes()).hexdigest()
    evidence["project_root"] = str(test_root)
    evidence["data_directory"] = str(desktop.DATA)
    try:
        stage = "fixture"
        (test_root / "calc.py").write_text("def square(value):\n    return value\n", encoding="utf-8")
        (test_root / "check.py").write_text(
            "from calc import square\nfrom classify import classify\n"
            "assert square(-3) == 9\nassert square(2.5) == 6.25\n"
            "assert classify(4) == 'even'\nassert classify(3) == 'odd'\n"
            "print('4 coordinator acceptance assertions passed')\n",
            encoding="utf-8",
        )
        original_check_hash = hashlib.sha256((test_root / "check.py").read_bytes()).hexdigest()
        subprocess.run(["git", "init", "-q"], cwd=test_root, check=True, timeout=30)
        subprocess.run(["git", "config", "user.name", "Coordinator Test"], cwd=test_root, check=True, timeout=30)
        subprocess.run(["git", "config", "user.email", "coordinator-test@localhost"], cwd=test_root, check=True, timeout=30)
        subprocess.run(["git", "add", "calc.py", "check.py"], cwd=test_root, check=True, timeout=30)
        subprocess.run(["git", "commit", "-q", "-m", "isolated coordinator fixture"], cwd=test_root, check=True, timeout=30)
        evidence["project_baseline"] = subprocess.run(["git", "rev-parse", "HEAD"], cwd=test_root, capture_output=True, text=True, check=True, timeout=30).stdout.strip()

        stage = "launch"
        with sync_playwright() as playwright:
            proc, endpoint = desktop.launch()
            browser = playwright.chromium.connect_over_cdp(endpoint)
            page = desktop.page_for(browser)
            stage = "connect_and_catalog"
            catalogs = {
                "codex-win": connected_models(page, "codex-win", "codex"),
                "dsh-win": connected_models(page, "dsh-win", "dsh"),
                "hermes-win": connected_models(page, "hermes-win", "hermes"),
            }
            models = {
                "codex-win": choose_model(catalogs["codex-win"], "luna"),
                "dsh-win": choose_model(catalogs["dsh-win"], "daily_two"),
                "hermes-win": choose_model(catalogs["hermes-win"], "daily_two"),
            }
            evidence["approved_models"] = {agent: {"id": model["id"], "provider_id": model.get("provider_id"), "provider_name": model.get("provider_name"), "effort": "low"} for agent, model in models.items()}

            stage = "create_isolated_project"
            project = desktop.ipc(page, "register_project", name="隔离管家协作验收", root=str(test_root))
            room = groups.group(page, "管家持续调度验收", ["codex-win", "dsh-win", "hermes-win"])
            desktop.ipc(page, "bind_project", conversationId=room, projectId=project["id"])
            group_detail = desktop.ipc(page, "get_conversation", id=room)
            project_chats = {chat["agent_id"]: chat["conversation_id"] for chat in group_detail["project_agent_chats"]}
            for agent in ("codex-win", "dsh-win", "hermes-win"):
                desktop.ipc(page, "set_session_settings", id=project_chats[agent], agentId=agent, model=models[agent]["id"], reasoningEffort="low")

            stage = "select_codex_coordinator"
            set_coordinator(page, room, "codex-win", models["codex-win"])
            desktop.ipc(page, "disconnect_dsh")
            desktop.ipc(page, "disconnect_hermes")
            check("workers may be disconnected when coordinator accepts the project", desktop.ipc(page, "dsh_status")["connection"] != "connected" and desktop.ipc(page, "hermes_status")["connection"] != "connected")

            request = (
                "先由Codex只读检查当前calc.py并给出简短方案，不要改文件。"
                "方案返回后，Codex再只修改calc.py，实现square(value)返回平方。"
                "方案返回后，DSH只新增classify.py，实现classify(value)返回\"even\"或\"odd\"。"
                "两个实现完成后，由DSH只运行python -B check.py验证；失败时报告并停止，不修改源码或check.py。"
                "验证通过后，由Hermes只新增CLOSEOUT.md，记录实际修改文件、验证命令和结果。"
                "按以上顺序分派，不得改check.py或新增其他文件。所有成员沿用各自项目配置模型。"
            )
            stage = "main_coordinator_workflow"
            workflow_id, started = start(page, room, request)
            check("dynamic workflow starts with one coordinator selection while workers were disconnected", started.get("coordination") is not None)
            job = wait_workflow(page, room, workflow_id)
            validate_main_workflow(page, room, job, request, test_root, models, original_check_hash)
            evidence["main_workflow"] = workflow_evidence(job, request)

            stage = "hermes_coordinator_smoke"
            run_manager_smoke(page, room, "hermes-win", "hermes-win", models, test_root)
            stage = "dsh_coordinator_smoke"
            run_manager_smoke(page, room, "dsh-win", "dsh-win", models, test_root)

            check("window reported no JavaScript errors", not desktop.errors)
            success = True
    except BaseException as exc:
        failure_category = safe_failure(exc)
        print("FAIL " + stage + " (" + failure_category + ")", flush=True)
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
        workflows = {
            item.get("workflow_id"): item.get("attempts", [])
            for item in evidence.values()
            if isinstance(item, dict) and isinstance(item.get("workflow_id"), str)
        }
        if last_workflow is not None and isinstance(last_workflow.get("id"), str):
            workflows.setdefault(last_workflow["id"], last_workflow.get("attempts", []))
        native_turn_attempts = sum(len(attempts) for attempts in workflows.values())
        report = {
            "success": success,
            "passed": len(checks),
            "checks": checks,
            "failure_stage": None if success else stage,
            "failure_category": failure_category,
            "failure_check": failure_check,
            "failure_details": failure_details,
            "failure_diagnostics": failure_diagnostics(last_workflow) if not success else None,
            "native_turn_attempts": native_turn_attempts,
            "native_turn_attempts_note": "Harness app-server turn attempts; provider-internal calls are not counted.",
            "model_requests": None,
            "model_requests_note": "Provider-internal model requests are not measured by this harness.",
            "provider_switches": 0,
            "sol_calls": 0,
            "evidence": evidence,
        }
        REPORT.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps({"success": success, "passed": len(checks), "failure_stage": report["failure_stage"], "failure_category": failure_category, "report": str(REPORT)}, ensure_ascii=False), flush=True)
        if test_root.exists():
            try:
                expected = Path(tempfile.gettempdir())
                resolved = test_root.resolve()
                if resolved.parent == expected.resolve() and resolved.name.startswith("tongxi-coordinator-project-"):
                    shutil.rmtree(resolved)
            except Exception:
                pass
        try:
            data_root = desktop.DATA.resolve()
            expected = Path(tempfile.gettempdir()).resolve()
            if data_root.parent == expected and data_root.name.startswith("tongxi-coordinator-data-"):
                shutil.rmtree(data_root)
        except Exception:
            pass


if __name__ == "__main__":
    run()
