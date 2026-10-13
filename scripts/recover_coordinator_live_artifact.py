"""Rebuild safe workflow evidence from the disposable live-test SQLite database.

Reads only the isolated database recorded in the live report and never exports
tool inputs or outputs. It is useful when the live test exits before its normal
workflow serializer runs.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import sqlite3
import tempfile

ROOT = Path(__file__).resolve().parents[1]
REPORT = ROOT / "artifacts" / "coordinator-live-verification.json"
NON_TOOL_KINDS = {"subAgentActivity", "enteredReviewMode", "exitedReviewMode", "contextCompaction"}


def main() -> None:
    report = json.loads(REPORT.read_text(encoding="utf-8"))
    data = Path(report["evidence"]["data_directory"]).resolve()
    project = Path(report["evidence"]["project_root"]).resolve()
    temp = Path(tempfile.gettempdir()).resolve()
    if data.parent != temp or not data.name.startswith("tongxi-coordinator-data-"):
        raise SystemExit("refusing unexpected isolated data path")
    if project.parent != temp or not project.name.startswith("tongxi-coordinator-project-"):
        raise SystemExit("refusing unexpected isolated project path")
    if not data.joinpath("hub.db").is_file():
        raise SystemExit("isolated evidence database is unavailable")

    db_uri = data.joinpath("hub.db").as_uri() + "?mode=ro"
    with sqlite3.connect(db_uri, uri=True) as db:
        db.row_factory = sqlite3.Row
        workflow_id = report["failure_diagnostics"]["workflow_id"]
        workflow_row = db.execute(
            "SELECT conversation_id,user_message_id,status FROM workflows WHERE id=?", (workflow_id,)
        ).fetchone()
        if not workflow_row:
            raise SystemExit("isolated workflow row is unavailable")
        routes = [dict(row) for row in db.execute(
            "SELECT r.task_id,r.workflow_id,r.purpose,r.source_id,r.quote,r.context_ids,t.agent_id "
            "FROM coordination_routes r LEFT JOIN project_tasks t ON t.id=r.task_id "
            "WHERE r.workflow_id=? ORDER BY t.position", (workflow_id,)
        )]
        tasks = [dict(row) for row in db.execute(
            "SELECT id,position,agent_id,status,depends_on,model,effort FROM project_tasks "
            "WHERE workflow_id=? ORDER BY position", (workflow_id,)
        )]
        attempts = [dict(row) for row in db.execute(
            "SELECT id,task_id,agent_id,stage,status,native_thread_id,model,reasoning_effort,activity "
            "FROM project_attempts WHERE workflow_id=? ORDER BY created_at,rowid", (workflow_id,)
        )]
        safe_attempts = []
        for attempt in attempts:
            events = [dict(row) for row in db.execute(
                "SELECT kind,title,status,input,output FROM project_attempt_events "
                "WHERE attempt_id=? ORDER BY created_at,rowid", (attempt["id"],)
            )]
            safe_attempts.append({
                "agent_id": attempt["agent_id"], "activity": attempt["activity"], "stage": attempt["stage"],
                "status": attempt["status"], "task_id": attempt["task_id"], "model": attempt["model"],
                "reasoning_effort": attempt["reasoning_effort"],
                "native_thread_id_sha256": hashlib.sha256((attempt["native_thread_id"] or "").encode()).hexdigest()[:16],
                "tool_event_count": len(events),
                "manager_tool_event_count": sum(event["kind"] not in NON_TOOL_KINDS for event in events) if attempt["activity"] == "coordinate" else None,
                "tool_events": [{
                    "kind": event["kind"], "title": event["title"], "status": event["status"],
                    "input_sha256": hashlib.sha256((event["input"] or "").encode()).hexdigest(),
                    "output_sha256": hashlib.sha256((event["output"] or "").encode()).hexdigest(),
                } for event in events],
            })
        manager_attempts = [attempt for attempt in safe_attempts if attempt["activity"] == "coordinate"]
        old_guard_events = [event for attempt in manager_attempts for event in attempt["tool_events"]]
        actual_manager_tools = [event for event in old_guard_events if event["kind"] not in NON_TOOL_KINDS]
        main = {
            "workflow_id": workflow_id,
            "status": workflow_row["status"],
            "routes": routes,
            "tasks": tasks,
            "attempts": safe_attempts,
        }
        recovered = {
            "source": "read-only isolated hub.db; lifecycle events retained and classified by local schema",
            "workflow_id": workflow_id,
            "raw_manager_event_count": len(old_guard_events),
            "raw_manager_event_kinds": [event["kind"] for event in old_guard_events],
            "actual_manager_tool_event_count": len(actual_manager_tools),
            "model_attempt_count": len(attempts),
            "all_attempts_use_approved_model_low": all(
                attempt["model"] == report["evidence"]["approved_models"][attempt["agent_id"]]["id"]
                and attempt["reasoning_effort"] == "low"
                and "sol" not in attempt["model"].lower()
                for attempt in attempts
            ),
            "all_tasks_completed": bool(tasks) and all(task["status"] == "completed" for task in tasks),
            "ordering_checks": report["evidence"]["main_prevalidation"]["ordering_checks"],
            "solution_branch_diffs": report["evidence"].get("solution_branch_diffs", []),
            "check_py_execution": report["evidence"]["main_prevalidation"]["check_py_execution"],
            "check_py_unchanged": report["evidence"]["main_prevalidation"]["check_py_sha256_matches_original"],
            "git": report["evidence"]["main_prevalidation"]["git"],
            "session_isolation": report["evidence"]["main_prevalidation"]["session_isolation"],
            "only_failure_was_legacy_raw_event_count_guard": bool(old_guard_events) and all(event["kind"] in NON_TOOL_KINDS for event in old_guard_events) and not actual_manager_tools,
            "legacy_failure_label": "manager turn produced no tool calls",
        }
        report["evidence"]["main_workflow"] = main
        report["evidence"]["offline_main_workflow_validation"] = recovered
        report["failure_check"] = recovered["legacy_failure_label"]
        report["failure_details"] = {
            "observed_raw_manager_event_count": len(old_guard_events),
            "observed_raw_manager_event_kinds": recovered["raw_manager_event_kinds"],
            "filtered_actual_manager_tool_event_count": len(actual_manager_tools),
        }
    REPORT.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({
        "workflow_id": recovered["workflow_id"],
        "tasks_completed": recovered["all_tasks_completed"],
        "models_low_passed": recovered["all_attempts_use_approved_model_low"],
        "ordering": recovered["ordering_checks"],
        "raw_manager_events": recovered["raw_manager_event_kinds"],
        "actual_manager_tool_events": recovered["actual_manager_tool_event_count"],
        "check_py": recovered["check_py_execution"],
        "session_isolation": bool(recovered["session_isolation"]),
        "legacy_failure_was_only_metadata": recovered["only_failure_was_legacy_raw_event_count_guard"],
        "report": str(REPORT),
    }, ensure_ascii=False))


if __name__ == "__main__":
    main()
