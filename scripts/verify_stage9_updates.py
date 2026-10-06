#!/usr/bin/env python3
"""Stage9 desktop/Rust IPC update-transaction verification (real IPC, no models)."""
import argparse
import hashlib
import json
import os
import sys
import traceback
import uuid
from pathlib import Path

import smoke_desktop as d
from playwright.sync_api import sync_playwright

SERVICES = {
    "dsh-win": {
        "installation_env": "AGENT_HUB_DSH_INSTALLATION",
        "installation_default": "D:/AI/dsh/bin/node_modules/@deepseek-ai/dsh",
        "connect_command": "connect_dsh",
        "package": "@deepseek-ai/dsh",
        "report": "stage9-updates-verification.json",
    },
    "codex-win": {
        "installation_env": "AGENT_HUB_CODEX_INSTALLATION",
        "installation_default": "D:/AI/_tools/npm-global/node_modules/@openai/codex",
        "connect_command": "connect_codex",
        "package": "@openai/codex",
        "report": "stage9-codex-updates-verification.json",
    },
}


def check(checks, name, ok, detail=None):
    item = {"name": name, "ok": bool(ok)}
    if detail is not None:
        item["detail"] = detail
    checks.append(item)
    if not item["ok"]:
        raise AssertionError(name)


def snap_version(res):
    return res["version"]


def connected(res):
    return res["connection"] == "connected"


def main():
    parser = argparse.ArgumentParser(description="隔离验收成员更新、切换和回滚")
    parser.add_argument("exe", nargs="?", default=str(d.EXE))
    parser.add_argument("--service", choices=sorted(SERVICES), default="dsh-win")
    args = parser.parse_args()
    service_id = args.service
    service = SERVICES[service_id]
    manifest = Path(os.environ.get(service["installation_env"], service["installation_default"])) / "package.json"
    report_name = service["report"]
    connect_command = service["connect_command"]
    exe = Path(args.exe).resolve()
    d.EXE = exe
    d.DATA = d.ARTIFACTS / ("stage9-updates-" + uuid.uuid4().hex)
    checks, error, proc, browser, baseline = [], None, None, None, None
    report = {"success": False, "passed": 0, "checks": checks, "model_requests": 0,
              "exe_sha256": None, "exe_bytes": None, "DATA": str(d.DATA),
              "test_data_directory": str(d.DATA), "errors_count": 1, "error": None}
    try:
        report["exe_sha256"] = hashlib.sha256(exe.read_bytes()).hexdigest()
        report["exe_bytes"] = exe.stat().st_size
        baseline = hashlib.sha256(manifest.read_bytes()).hexdigest()
        proc, endpoint = d.launch()
        with sync_playwright() as pw:
            browser = pw.chromium.connect_over_cdp(endpoint)
            try:
                page = d.page_for(browser)
                page.set_default_timeout(600000)

                svc = d.ipc(page, "check_service_update", serviceId=service_id)
                latest = svc["latest_version"]
                check(checks, "check_service_update.latest_version", bool(latest), latest)
                check(checks, "check_service_update.check_source",
                      svc["check_source"] == "npm-registry", svc["check_source"])
                update_available = svc["update_available"]
                check(checks, "check_service_update.update_available",
                      update_available is True or (service_id == "codex-win" and update_available is False),
                      update_available)

                s0 = d.ipc(page, "service_update_status", serviceId=service_id)
                check(checks, "status.initial.managed_false", s0["managed"] is False)
                check(checks, "status.initial.ready_none", s0["ready_plan_id"] is None)

                old = snap_version(d.ipc(page, connect_command))
                check(checks, connect_command + ".version_recorded", bool(old), old)
                if service_id == "codex-win" and update_available is False:
                    installed = svc.get("installed_version")
                    check(checks, "codex.installed_matches_registry_latest",
                          bool(installed) and installed == latest and latest in old,
                          {"installed": installed, "latest": latest, "runtime": old})
                    check(checks, "codex.no_candidate_when_already_current",
                          s0["ready_plan_id"] is None and s0["active_version"] is None)
                    check(checks, "manifest_unchanged",
                          hashlib.sha256(manifest.read_bytes()).hexdigest() == baseline)
                    return
                # 非受管状态没有槽位版本可报（实现语义：生效版本只认受管槽 manifest），
                # 外部安装的运行版本已经由 connect 响应记录，不再混入受管状态。
                check(checks, "status.initial.active_version_none",
                      s0["active_version"] is None, s0["active_version"])

                prep = d.ipc(page, "prepare_service_update", serviceId=service_id)
                plan_id, candidate = prep["plan_id"], prep["candidate_version"]
                check(checks, "prepare_service_update.supported", prep["supported"] is True)
                check(checks, "prepare_service_update.plan_id", bool(plan_id), plan_id)
                check(checks, "prepare_service_update.candidate_version", bool(candidate), candidate)

                s1 = d.ipc(page, "service_update_status", serviceId=service_id)
                check(checks, "status.prepared.managed_false", s1["managed"] is False)
                check(checks, "status.prepared.ready_plan_id", s1["ready_plan_id"] == plan_id)
                check(checks, connect_command + ".version_after_prepare_old",
                      snap_version(d.ipc(page, connect_command)) == old)

                app = d.ipc(page, "apply_service_update", serviceId=service_id, planId=plan_id)
                check(checks, "apply.snapshot_version_candidate",
                      snap_version(app) == candidate, snap_version(app))
                check(checks, "apply.connection_connected", connected(app))
                s2 = d.ipc(page, "service_update_status", serviceId=service_id)
                check(checks, "status.applied.managed_true", s2["managed"] is True)
                check(checks, "status.applied.ready_none", s2["ready_plan_id"] is None)
                check(checks, "status.applied.rollback_true", s2["rollback_available"] is True)

                rb = d.ipc(page, "rollback_service_update", serviceId=service_id)
                check(checks, "rollback.snapshot_version_old",
                      snap_version(rb) == old, snap_version(rb))
                check(checks, "rollback.connection_connected", connected(rb))
                s3 = d.ipc(page, "service_update_status", serviceId=service_id)
                check(checks, "status.rolled_back.managed_false", s3["managed"] is False)
                check(checks, "status.rolled_back.rollback_false", s3["rollback_available"] is False)

                check(checks, "manifest_unchanged",
                      hashlib.sha256(manifest.read_bytes()).hexdigest() == baseline)
            finally:
                browser.close()
    except Exception as exc:
        error = exc
        report["error"] = {"class": type(exc).__name__,
                           "traceback": [f"{Path(f).name}:{n}"
                                         for f, n, *_ in traceback.extract_tb(exc.__traceback__)]}
    finally:
        if proc is not None and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except Exception:
                proc.kill()
        passed_names = [c["name"] for c in checks if c["ok"]]
        report["passed"] = len(passed_names)
        report["errors_count"] = 1 if error is not None else 0
        report["success"] = (error is None and bool(checks)
                             and len(passed_names) == len(checks))
        report["service_id"] = service_id
        (d.ARTIFACTS / report_name).write_text(json.dumps(report, indent=2), encoding="utf-8")
    if error is not None:
        print(json.dumps({"error": {"class": type(error).__name__}}))
        sys.exit(1)


if __name__ == "__main__":
    main()
