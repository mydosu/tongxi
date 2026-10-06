#!/usr/bin/env python3
"""Read-only update-source verification for all four managed services."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import sys
import traceback

from playwright.sync_api import sync_playwright

import smoke_desktop as desktop

ROOT = Path(__file__).resolve().parents[1]
REPORT = ROOT / "artifacts" / "service-update-inventory-verification.json"
PACKAGES = {
    "dsh-win": (
        os.environ.get("AGENT_HUB_DSH_INSTALLATION")
        or "D:/AI/dsh/bin/node_modules/@deepseek-ai/dsh"
    ),
    "codex-win": (
        os.environ.get("AGENT_HUB_CODEX_INSTALLATION")
        or "D:/AI/_tools/npm-global/node_modules/@openai/codex"
    ),
}
EXPECTED_SOURCES = {
    "dsh-win": "npm-registry",
    "codex-win": "npm-registry",
    "hermes-win": "hermes-update",
    "albion-wsl": "local",
}


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check(checks: list[dict], name: str, ok: bool, detail=None) -> None:
    checks.append({"name": name, "ok": bool(ok), "detail": detail})
    if not ok:
        raise AssertionError(name)


def main() -> None:
    checks: list[dict] = []
    service_results: dict[str, dict] = {}
    js_errors: list[str] = []
    error = None
    proc = None
    package_hashes = {
        service_id: sha256(Path(installation) / "package.json")
        for service_id, installation in PACKAGES.items()
    }
    hermes_repo = Path(os.environ.get(
        "AGENT_HUB_HERMES_REPO",
        Path(os.environ.get("LOCALAPPDATA", "")) / "hermes" / "hermes-agent",
    ))
    hermes_status_before = None
    if (hermes_repo / ".git").is_dir():
        import subprocess
        hermes_status_before = subprocess.run(
            ["git", "status", "--porcelain"], cwd=hermes_repo,
            capture_output=True, check=True, text=True,
        ).stdout

    report = {
        "success": False,
        "passed": 0,
        "checks": checks,
        "services": service_results,
        "model_requests": 0,
        "executable_sha256": None,
        "executable_bytes": None,
        "javascript_errors": js_errors,
        "error": None,
    }
    try:
        exe = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else desktop.EXE.resolve()
        desktop.EXE = exe
        report["executable_sha256"] = sha256(exe)
        report["executable_bytes"] = exe.stat().st_size
        proc, endpoint = desktop.launch()
        with sync_playwright() as playwright:
            browser = playwright.chromium.connect_over_cdp(endpoint)
            try:
                page = desktop.page_for(browser)
                page.on("pageerror", lambda exc: js_errors.append(str(exc)))
                for service_id, expected_source in EXPECTED_SOURCES.items():
                    result = desktop.ipc(
                        page, "check_service_update", serviceId=service_id
                    )
                    service_results[service_id] = {
                        "check_source": result["check_source"],
                        "installed_version": result["installed_version"],
                        "latest_version": result["latest_version"],
                        "update_available": result["update_available"],
                        "version_source": result["version_source"],
                    }
                    check(
                        checks,
                        f"{service_id}.uses_expected_update_source",
                        result["check_source"] == expected_source,
                        result["check_source"],
                    )
                    if service_id in ("dsh-win", "codex-win"):
                        check(
                            checks,
                            f"{service_id}.has_real_registry_version_comparison",
                            isinstance(result["latest_version"], str)
                            and isinstance(result["update_available"], bool),
                        )
                    elif service_id == "hermes-win":
                        check(
                            checks,
                            "hermes-win.does_not_invent_semver_latest",
                            result["latest_version"] is None
                            and isinstance(result["update_available"], bool),
                        )
                    else:
                        check(
                            checks,
                            "albion-wsl.is_local_only",
                            result["latest_version"] is None
                            and result["update_available"] is None,
                        )
                for service_id, installation in PACKAGES.items():
                    check(
                        checks,
                        f"{service_id}.external_manifest_unchanged",
                        sha256(Path(installation) / "package.json")
                        == package_hashes[service_id],
                    )
                if hermes_status_before is not None:
                    import subprocess
                    hermes_status_after = subprocess.run(
                        ["git", "status", "--porcelain"], cwd=hermes_repo,
                        capture_output=True, check=True, text=True,
                    ).stdout
                    check(
                        checks,
                        "hermes-win.read_only_check_did_not_change_worktree",
                        hermes_status_after == hermes_status_before,
                    )
                check(checks, "no_javascript_runtime_errors", not js_errors)
                report["success"] = True
            finally:
                browser.close()
    except Exception as exc:
        error = exc
        report["error"] = {
            "class": type(exc).__name__,
            "traceback": [
                f"{Path(frame).name}:{line}"
                for frame, line, *_ in traceback.extract_tb(exc.__traceback__)
            ],
        }
    finally:
        if proc is not None and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except Exception:
                proc.kill()
        report["passed"] = sum(1 for item in checks if item["ok"])
        REPORT.parent.mkdir(parents=True, exist_ok=True)
        REPORT.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    if error is not None:
        sys.exit(1)


if __name__ == "__main__":
    main()
