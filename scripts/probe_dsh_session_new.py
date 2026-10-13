"""No-prompt DSH ACP initialization/session creation diagnostic.

Only `initialize` and `session/new` are sent. The isolated DSH home and native
bridge log stay in the generated Temp directory for post-run inspection. No
native error message, configuration, credential, or header is emitted.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import queue
import re
import subprocess
import sys
import tempfile
import threading
import time
import uuid

import local_paths

ROOT = Path(__file__).resolve().parents[1]
KNOWN_TERMS = (
    "undefined", "not a function", "not found", "not registered", "missing",
    "sessionprojection", "sessionquery", "sessionpersistence", "agents", "sessions",
    "agentloop", "provider", "model", "workspace", "cwd", "schema", "connection", "closed",
    "serialize", "restore", "protocol", "persist", "tools", "permission", "sandbox",
    "read-only", "shell", "pwsh", "subprocess", "inject", "services", "service",
    "pending", "activate", "activation", "entry", "entries", "filesystem", "path",
)
FRAME_RE = re.compile(r"@deepseek-ai[/\\][a-z0-9-]+[/\\]lib[/\\][a-z0-9._/-]+:\d+:\d+", re.I)
CLASS_RE = re.compile(r"\b([A-Z][A-Za-z0-9]*(?:Error|Exception))\b")


def safe_error(error: dict) -> dict:
    # Do not serialize message/data. They can contain arbitrary native detail.
    raw = json.dumps(error, ensure_ascii=False).lower()
    message = str(error.get("message", ""))
    class_match = CLASS_RE.search(message)
    return {
        "errorClass": class_match.group(1) if class_match else "JsonRpcError",
        "dshPackageFrames": sorted(set(FRAME_RE.findall(raw))),
        "knownTerms": [term for term in KNOWN_TERMS if term in raw],
    }


def safe_bridge_log(path: Path) -> dict | None:
    if not path.is_file():
        return None
    # The full stack is deliberately kept in the isolated Temp fixture. Parse
    # it in memory and persist only the class, DSH package frames, and terms.
    try:
        raw = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    class_match = CLASS_RE.search(raw)
    frames = sorted(set(FRAME_RE.findall(raw)))
    error_class = class_match.group(1) if class_match else "UnknownErrorClass"
    if error_class == "UnknownErrorClass" and any(frame.endswith("dsh-app-boot/lib/index.js:1511:9") for frame in frames):
        # Installed app-boot source at this frame throws `new Error(...)`.
        error_class = "Error"
    return {
        "errorClass": error_class,
        "dshPackageFrames": frames,
        "knownTerms": [term for term in KNOWN_TERMS if term in raw.lower()],
    }


def dsh_acp_contract(installation: Path, bridge: str) -> dict:
    package = installation / "node_modules" / "@deepseek-ai" / "dsh-acp"
    manifest = json.loads((package / "package.json").read_text(encoding="utf-8"))
    source = (package / "lib" / "index.js").read_text(encoding="utf-8")
    match = re.search(r"const inject = \[([\s\S]*?)\];", source)
    required = re.findall(r"\"([A-Za-z][A-Za-z0-9]*)\"", match.group(1)) if match else []
    configured = re.search(r"entries\.push\(\{ id: 'acp',[\s\S]*?inject: \[([^\]]*)\]", bridge)
    injected = re.findall(r"['\"]([A-Za-z][A-Za-z0-9]*)['\"]", configured.group(1)) if configured else []
    return {
        "package": manifest.get("name"),
        "version": manifest.get("version"),
        "packageDeclaredInject": required,
        "bridgeEntryInject": injected,
        "entryInjectDelta": {
            "packageOnly": [item for item in required if item not in injected],
            "entryOnly": [item for item in injected if item not in required],
        },
        "runtimeInterpretation": "not_runtime_missing",
        "scopeNote": "Static package metadata versus bridge entry declaration only; not evidence of unavailable runtime services.",
    }


class Client:
    def __init__(self, proc: subprocess.Popen):
        self.proc = proc
        self.incoming: queue.Queue = queue.Queue()
        self.messages: list[str] = []
        self.safe_diagnostics: list[dict] = []
        self.next_id = 0

        def stdout_reader() -> None:
            for line in proc.stdout:
                try:
                    self.incoming.put(json.loads(line))
                except (ValueError, TypeError):
                    continue
            self.incoming.put(None)

        def stderr_reader() -> None:
            # Bridge stderr diagnostics are already projected by its own filter.
            for line in proc.stderr:
                try:
                    value = json.loads(line)
                    if isinstance(value, list):
                        for item in value:
                            if not isinstance(item, dict):
                                continue
                            frames = [frame for frame in item.get("frames", []) if FRAME_RE.fullmatch(str(frame))]
                            terms = [term for term in item.get("knownTerms", []) if term in KNOWN_TERMS]
                            error_class = item.get("errorClass")
                            if error_class and re.fullmatch(r"[A-Za-z][A-Za-z0-9]*(?:Error|Exception)", str(error_class)):
                                self.safe_diagnostics.append({"errorClass": error_class, "dshPackageFrames": frames, "knownTerms": terms})
                except (ValueError, TypeError):
                    # Never print or persist arbitrary stderr.
                    continue

        threading.Thread(target=stdout_reader, daemon=True).start()
        threading.Thread(target=stderr_reader, daemon=True).start()

    def rpc(self, method: str, params: dict, timeout: int = 90) -> dict:
        self.next_id += 1
        request_id = self.next_id
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}) + "\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                value = self.incoming.get(timeout=1)
            except queue.Empty:
                continue
            if value is None:
                raise RuntimeError("bridge exited before JSON-RPC response")
            if isinstance(value, dict) and isinstance(value.get("method"), str):
                # Keep only fixed protocol method names; never retain update data.
                if value["method"] in {"session/update", "agent_hub/model_route"}:
                    self.messages.append(value["method"])
                continue
            if isinstance(value, dict) and value.get("id") == request_id:
                if isinstance(value.get("error"), dict):
                    raise ProbeRpcError(method, safe_error(value["error"]))
                result = value.get("result")
                if not isinstance(result, dict):
                    raise RuntimeError("unexpected protocol response shape")
                return result
        raise TimeoutError(method)


class ProbeRpcError(RuntimeError):
    def __init__(self, method: str, safe: dict):
        super().__init__(method)
        self.method = method
        self.safe = safe


def run() -> int:
    installation = Path(local_paths.dsh_installation()).resolve()
    if not installation.is_dir():
        raise SystemExit("DSH installation unavailable")
    bridge = (ROOT / "src-tauri" / "src" / "dsh_bridge.mjs").read_text(encoding="utf-8")
    contract = dsh_acp_contract(installation, bridge)
    run_root = Path(tempfile.mkdtemp(prefix="tongxi-dsh-session-new-probe-")).resolve()
    app_data = run_root / "app-data"
    app_data.mkdir()
    # Mirror manager_directory(db, workflow, "dsh-win") from the app:
    # <hub.db parent>/coordinators/<workflow>/dsh-win/workspace.
    workspace = app_data / "coordinators" / str(uuid.uuid4()) / "dsh-win" / "workspace"
    workspace.mkdir(parents=True)
    data = workspace / "dsh-native"
    home = data

    env = os.environ.copy()
    env["AGENT_HUB_DSH_HOME"] = str(home)
    env.pop("AGENT_HUB_DSH_PROJECT_MODE", None)
    env["AGENT_HUB_COORDINATOR_MODE"] = "1"
    env["AGENT_HUB_DSH_DIAGNOSTICS"] = "1"
    env["AGENT_HUB_DSH_AUTH_BRIDGE"] = (ROOT / "src-tauri" / "src" / "dsh_hermes_auth.py").read_text(encoding="utf-8")
    env["AGENT_HUB_DSH_FALLBACK_BRIDGE"] = (ROOT / "src-tauri" / "src" / "dsh_fallback.mjs").read_text(encoding="utf-8")

    proc = subprocess.Popen(
        ["node", "--input-type=module", "-e", bridge, str(installation), str(data)],
        cwd=workspace,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    client = Client(proc)
    failure: dict | None = None
    initialize_ok = False
    session_new_ok = False
    notifications = 0
    try:
        init = client.rpc("initialize", {
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False},
            "clientInfo": {"name": "tongxi-session-new-probe", "version": "1"},
        })
        initialize_ok = isinstance(init.get("agentInfo"), dict)
        new = client.rpc("session/new", {"cwd": str(workspace), "mcpServers": []})
        session_new_ok = isinstance(new.get("sessionId"), str) and bool(new.get("sessionId"))
        notifications = len(client.messages)
    except ProbeRpcError as exc:
        failure = {"stage": exc.method, **exc.safe}
    except TimeoutError:
        failure = {"stage": "timeout", "errorClass": "TimeoutError", "dshPackageFrames": [], "knownTerms": []}
    except BaseException as exc:
        cls = type(exc).__name__
        failure = {"stage": "bridge", "errorClass": cls if re.fullmatch(r"[A-Za-z][A-Za-z0-9]*(?:Error|Exception)", cls) else "RuntimeError", "dshPackageFrames": [], "knownTerms": []}
    finally:
        if proc.stdin and not proc.stdin.closed:
            try:
                proc.stdin.close()
            except OSError:
                pass
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.terminate()
            try:
                proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)

    log_path = data / "bridge-error.log"
    log_diagnostics = safe_bridge_log(log_path)
    if log_diagnostics and log_diagnostics not in client.safe_diagnostics:
        client.safe_diagnostics.append(log_diagnostics)
    report = {
        "initialize_ok": initialize_ok,
        "session_new_ok": session_new_ok,
        "notification_count": notifications,
        "prompt_sent": False,
        "probe_storage_retained": True,
        "data_directory": str(run_root),
        "bridge_error_log_present": log_path.is_file(),
        "acp_injection_contract": contract,
        "rpc_failure": failure,
        "sanitized_bridge_diagnostics": client.safe_diagnostics,
    }
    output = ROOT / "artifacts" / "dsh-session-new-probe-safe.json"
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({
        "initialize_ok": initialize_ok,
        "session_new_ok": session_new_ok,
        "prompt_sent": False,
        "rpc_failure": failure,
        "bridge_diagnostics": client.safe_diagnostics,
        "package_declared_inject": contract["packageDeclaredInject"],
        "bridge_entry_inject": contract["bridgeEntryInject"],
        "entry_inject_delta": contract["entryInjectDelta"],
        "runtime_interpretation": contract["runtimeInterpretation"],
        "safe_report": str(output.relative_to(ROOT)),
        "retained_logs": log_path.is_file(),
    }, ensure_ascii=False), flush=True)
    return 0 if session_new_ok else 1


if __name__ == "__main__":
    raise SystemExit(run())
