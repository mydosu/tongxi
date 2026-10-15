"""Static contract checks for the persistent project coordinator backend.

This does not start native agents or call model APIs. Runtime/database behavior is
covered by src-tauri/src/coordinator_tests.rs.
"""
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]


def read(relative: str) -> str:
    return (ROOT / relative).read_text(encoding="utf-8")


def compact(text: str) -> str:
    return re.sub(r"\s+", "", text)


policy = read("src-tauri/src/coordinator_policy.rs")
store = read("src-tauri/src/coordinator_store.rs")
clients = read("src-tauri/src/coordinator_clients.rs")
codex = read("src-tauri/src/codex.rs")
hermes = read("src-tauri/src/hermes.rs")
hermes_bridge = read("src-tauri/src/hermes_bridge.py")
dsh = read("src-tauri/src/dsh.rs")
dsh_bridge = read("src-tauri/src/dsh_bridge.mjs")
schema = read("src-tauri/src/store.rs")
project_schema = read("src-tauri/src/project_store.rs")
coordinator = read("src-tauri/src/project_coordinator.rs")
projects = read("src-tauri/src/projects.rs")
project_native = read("src-tauri/src/project_native.rs")
frontend = read("src/main.ts")

checks = []


def check(label: str, condition: bool) -> None:
    if not condition:
        raise AssertionError(label)
    checks.append(label)


check("route and decision DTOs reject unknown fields", policy.count("#[serde(deny_unknown_fields)]") >= 4)
route_struct = re.search(r"pub struct Route\s*\{(.*?)\n\}", policy, re.S)
check("coordinator task briefs are bounded and yield to the user's words", route_struct is not None and re.search(r"\binstructions\s*:", route_struct.group(1)) is not None and "任务说明过长" in policy and "以用户原话为准" in policy)
check("quotes must occur verbatim in a source", "source.text.contains(&reference.quote)" in policy)
policy_compact = compact(policy)
check("assignments and model grants require user sources", policy_compact.count("source(sources,&reference,true)") >= 2)
check("user-named models win and unlisted models are rejected", "用户原话点名了模型，以用户为准" in policy and "这个模型不在该成员的模型目录里" in policy)
check("coordinator routes persist with source and quote", all(name in store for name in ("coordination_routes", "source_id", "quote", "context_ids")))
check("duplicate routes have a database uniqueness key", "UNIQUE(workflow_id,purpose,source_id,quote)" in store)
check("same agent workers are checked independently of coordinator runs", "COALESCE(activity,'')!='coordinate'" in store)
check("manager client data uses workflow and agent-specific directories", "self.root.join(id).join(agent)" in compact(clients))
check("Codex manager profile disables native tool families", all(flag in clients for flag in ("mcp_servers={}", "plugins={}", "features.shell_tool=false", "features.unified_exec=false", "features.apply_patch_freeform=false", "features.collab=false", "features.multi_agent=false", "features.skills=false", "web_search=\\\"disabled\\\"")))
check("Codex manager profile assigns approval decisions to the user", 'approval_reviewer=\\\"user\\\"' in clients)
check("native app-server lifecycle metadata is excluded from tool events", all(f'"{kind}"' in codex[codex.index("pub(crate) fn codex_tool_event"):codex.index("fn tool_content_text")] for kind in ("subAgentActivity", "enteredReviewMode", "exitedReviewMode", "contextCompaction")))
check("manager native turns reject real tool events on both start and completion", compact(project_native).count("ifmanager{state.invalid=true;return;}") >= 2)
check("Hermes manager bridge clears toolsets and hard-rejects execution", "kwargs['enabled_toolsets'] = []" in hermes_bridge and "agent.tools = []" in hermes_bridge and "agent._execute_tool_calls = types.MethodType(reject_tools, agent)" in hermes_bridge)
check("Hermes starts coordinator in a separate native process", "fn coordinator_client" in hermes and "AGENT_HUB_COORDINATOR_MODE" in hermes)
plugin_start = dsh_bridge.find("const plugins = [")
plugin_end = dsh_bridge.find("\n  ];", plugin_start)
plugin_block = dsh_bridge[plugin_start:plugin_end] if plugin_start >= 0 and plugin_end > plugin_start else ""
worker_plugin_ids = set(re.findall(r"\['([^']+)',\s*'dsh-[^']+'\]", plugin_block))
allowed_match = re.search(r"const allowed = new Set\(\[(.*?)\]\);", dsh_bridge, re.S)
manager_plugin_ids = set(re.findall(r"'([^']+)'", allowed_match.group(1))) if allowed_match else set()
check("DSH manager allowlist excludes permission, shell executor, and all tool plugins", bool(allowed_match) and not ({"permission", "pwsh-sandbox"} & manager_plugin_ids) and not any(plugin.startswith("tool-") for plugin in manager_plugin_ids))
check("DSH worker composition retains its complete native tool plugin set", {"permission", "pwsh-sandbox", "tool-pwsh", "tool-fs", "tool-subagent", "tool-web"} <= worker_plugin_ids and "if (coordinatorMode)" in dsh_bridge and "entries.splice(index,1)" in compact(dsh_bridge))
check("DSH coordinator uses read-only sandbox", "mode:'read-only'" in dsh_bridge)
check("DSH starts coordinator in a separate native process", "fn coordinator_client" in dsh and "AGENT_HUB_COORDINATOR_MODE" in dsh)
check("coordinator tables migrate with schema 17", "crate::coordinator_store::migrate(tx)?" in project_schema and "PRAGMA user_version=17;" in schema)
check("dynamic start does not require every planned worker to be connected", "create_with_connections(room,message,content,&roles,false)" in compact(coordinator))
busy_at = projects.find("pub fn agent_project_busy")
busy_end = projects.find("pub fn cancel(", busy_at)
busy_method = compact(projects[busy_at:busy_end])
check("service update busy guard includes coordinator attempts", busy_at >= 0 and "project_attempts" in busy_method and "statusIN('starting','running','cancelling')" in busy_method)
stamp_at = frontend.find("const jobs=roomProjects()")
stamp_end = frontend.find(";", stamp_at)
check("project progress repaint stamp tracks coordinator state", stamp_at >= 0 and "job.coordination" in frontend[stamp_at:stamp_end])
checkpoint_at = project_schema.find("pub fn checkpoint_attempt")
checkpoint_end = project_schema.find("#[cfg(test)]", checkpoint_at)
checkpoint_body = compact(project_schema[checkpoint_at:checkpoint_end])
check("native-complete coordinated tasks stay running until merge finalization", "attempt.status==\"completed\"&&attempt.activity.as_deref().is_some_and(|value|value.starts_with(\"task:\"))" in checkpoint_body and "\"running\"" in checkpoint_body)
batch_at = coordinator.find("fn run_coordinated_batch")
batch_end = coordinator.find("fn run_coordinated_task", batch_at)
batch_body = compact(coordinator[batch_at:batch_end])
check("coordinated task merge occurs before task completion publication", batch_at >= 0 and batch_end > batch_at and batch_body.find("project_worktree::merge") >= 0 and batch_body.find("project_worktree::merge") < batch_body.find("finish_coordinated_task"))

print(f"coordinator backend static checks: {len(checks)}/{len(checks)} passed")
for label in checks:
    print(f"PASS {label}")
