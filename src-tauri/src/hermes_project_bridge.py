"""Hermes ACP bridge for project work; exposes only the per-attempt Agent Hub MCP tools."""
import asyncio
import copy
import json
import os
from pathlib import Path
import sys

repo = Path(sys.argv[1]).resolve()
data = Path(sys.argv[2]).resolve()
data.mkdir(parents=True, exist_ok=True)
sys.path.insert(0, str(repo))

import hermes_bootstrap
hermes_bootstrap.harden_import_path()
from acp_adapter.entry import _load_env, _setup_logging
_setup_logging()
_load_env()

import acp
from acp.exceptions import RequestError
from acp.schema import SetSessionConfigOptionResponse
from acp_adapter.server import HermesACPAgent
from acp_adapter.session import SessionManager
from hermes_constants import parse_reasoning_effort
from hermes_state import SessionDB


class ProjectSessions(SessionManager):
    """Start every project session with no profile tools; ACP adds only its scoped MCP server."""

    def _make_agent(self, **kwargs):
        kwargs["enabled_toolsets"] = []
        kwargs["disabled_toolsets"] = []
        agent = super()._make_agent(**kwargs)
        agent.tools = []
        agent.valid_tool_names = set()
        # Project sessions use only the current group's prompt and scoped project MCP;
        # do not inject Hermes's private memory-provider context.
        if hasattr(agent, "_memory_manager"):
            agent._memory_manager = None
        agent._hub_default_reasoning = copy.deepcopy(agent.reasoning_config)
        agent._disable_streaming = False
        return agent


class ProjectACP(HermesACPAgent):
    async def _register_session_mcp_servers(self, state, mcp_servers):
        server_names = {server.name for server in mcp_servers or []}
        if server_names not in (set(), {"agent_hub"}):
            raise RequestError(-32602, "Project sessions allow only the scoped Agent Hub tools")
        await super()._register_session_mcp_servers(state, mcp_servers)

    async def set_config_option(self, config_id, session_id, value, **kwargs):
        if config_id != "reasoning_effort":
            raise RequestError(-32602, "Unsupported project session setting")
        state = await asyncio.to_thread(self.session_manager.get_session, session_id)
        if state is None:
            raise RequestError(-32602, "Unknown project session")
        with state.runtime_lock:
            if state.is_running or state.command_op:
                raise RequestError(-32603, "Project session busy")
            if value == "default":
                state.agent.reasoning_config = copy.deepcopy(state.agent._hub_default_reasoning)
            else:
                config = parse_reasoning_effort(value)
                if config is None:
                    raise RequestError(-32602, "Unsupported reasoning effort")
                state.agent.reasoning_config = config
        return SetSessionConfigOptionResponse(config_options=[])

    async def prompt(self, prompt, session_id, **kwargs):
        if any(block.type == "text" and block.text.lstrip().startswith("/") for block in prompt):
            raise RequestError(-32602, "Slash commands are disabled in project sessions")
        return await super().prompt(prompt=prompt, session_id=session_id, **kwargs)


# The bridge intentionally skips global MCP discovery. Per-session project MCP is attached later.
import hermes_cli.mcp_startup
hermes_cli.mcp_startup.start_background_mcp_discovery = lambda **kwargs: None
hermes_cli.mcp_startup.ensure_mcp_discovery_before_agent_build = lambda **kwargs: None

sessions = ProjectSessions(db=SessionDB(db_path=data / "sessions.db"))
agent = ProjectACP(sessions)
try:
    asyncio.run(acp.run_agent(agent, use_unstable_protocol=True))
finally:
    agent.session_manager.end_all_sessions()
