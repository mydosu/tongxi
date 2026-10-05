"""In-process extension of installed Hermes ACP; no changes to the installation.

Stdio contains protocol only. Native auth remains owned by Hermes. Histories
use the hub's own SessionDB; this milestone exposes a tool-free private chat.
"""
import asyncio
import copy
import json
import os
from pathlib import Path
import sys

repo = Path(sys.argv[1]).resolve()
data = Path(sys.argv[2]).resolve()
sys.path.insert(0, str(repo))
import hermes_bootstrap
hermes_bootstrap.harden_import_path()
from acp_adapter.entry import _load_env, _setup_logging
_setup_logging()
_load_env()
import acp
from acp.schema import SetSessionConfigOptionResponse
from acp_adapter.server import HermesACPAgent
from acp_adapter.session import SessionManager
from hermes_state import SessionDB
from hermes_constants import parse_reasoning_effort


class ChatSessions(SessionManager):
    """同席要当四个 agent 的**共同桌面端**：同一个会话库、同一条会话，谁那边看都一样。

    但 Hermes 的 ACP 适配器只认 `source='acp'`：`list_sessions` 过滤它，载入时
    `_restore` 里也写死 `row["source"] != "acp"` 就返回 None，于是她桌面端的历史会话
    在同席里既列不出也接不上。这里只在**本进程内**放宽这两处（绝不改数据库里的 source、
    不动她的安装）——hermes_state 的 INTERNAL_LISTING_SOURCES 决定哪些是内部会话，
    跟着它走就不会把 oneshot/tool/kanban 这类翻出来。
    """

    def _relaxed_db(self):
        db = self._get_db()
        if db is None:
            sys.stderr.write('AGENT_HUB_SHARED:' + json.dumps({'relax': 'no-db'}) + '\n')
            return db
        if getattr(db, "_hub_shared", False):
            return db
        from hermes_state_sessions import INTERNAL_LISTING_SOURCES

        original_get = db.get_session

        def get_session(session_id, *args, **kwargs):
            row = original_get(session_id, *args, **kwargs)
            if isinstance(row, dict) and row.get("source") not in (None, "acp") \
                    and row.get("source") not in INTERNAL_LISTING_SOURCES:
                # 仅供适配器判断来源；不写回数据库。
                return {**row, "source": "acp"}
            return row

        original_list = db.list_sessions_rich

        def list_sessions_rich(source=None, **kwargs):
            if source == "acp":
                try:
                    rows = list(original_list(
                        source=None, exclude_sources=list(INTERNAL_LISTING_SOURCES), **kwargs
                    ))
                    sys.stderr.write('AGENT_HUB_SHARED:' + json.dumps({'list_all': len(rows)}) + '\n')
                    return rows
                except Exception as exc:  # 不吞异常，暴露给 stderr 供排障
                    sys.stderr.write('AGENT_HUB_SHARED:' + json.dumps({'list_error': type(exc).__name__ + ': ' + str(exc)[:160]}) + '\n')
                    raise
            return original_list(source=source, **kwargs)

        db.get_session = get_session
        db.list_sessions_rich = list_sessions_rich
        db._hub_shared = True
        sys.stderr.write('AGENT_HUB_SHARED:' + json.dumps({'relax': str(getattr(db, 'db_path', '?'))}) + '\n')
        return db

    def _make_agent(self, **kwargs):
        self._relaxed_db()
        kwargs['enabled_toolsets'] = []
        kwargs['disabled_toolsets'] = []
        agent = super()._make_agent(**kwargs)
        agent.tools = []
        agent.valid_tool_names = set()
        # Runtime tool execution is disabled as well as tool advertisement.
        def tools_disabled(*args, **kw):
            raise RuntimeError('Tools disabled in Agent Hub private chat')
        agent._execute_tool_calls = tools_disabled
        agent._hub_default_reasoning = copy.deepcopy(agent.reasoning_config)
        # 思考块只从流式增量推给 ACP 客户端：profile 里 model.streaming=false 的会话
        # （例如阿尔比恩）非流式请求，Hermes 的非流式兜底又要求「未设 stream_delta_callback」，
        # 结果推理只落库、发不出来。这里只把本会话恢复成流式，不动 profile、不影响其他链路。
        agent._disable_streaming = False
        return agent


class ChatACP(HermesACPAgent):
    async def _attach_session_mcp(self, state, mcp_servers, log, *args):
        if mcp_servers:
            raise acp.RequestError(-32602, 'External MCP servers disabled in private chat')

    async def set_config_option(self, config_id, session_id, value, **kwargs):
        if config_id not in ('reasoning_effort', 'hub_development_digest'):
            raise acp.RequestError(-32602, 'Unsupported session setting')
        state = await asyncio.to_thread(self.session_manager.get_session, session_id)
        if state is None:
            raise acp.RequestError(-32602, 'Unknown session')
        with state.runtime_lock:
            if state.is_running or state.command_op:
                raise acp.RequestError(-32603, 'Session busy')
            if config_id == 'hub_development_digest':
                # Opt-in development digest. Only an Albion Agent exposes the
                # setter; any other (e.g. Windows Hermes) agent rejects this
                # setting. The text goes to the agent's ephemeral system slot
                # and is never persisted, cached, or echoed back here.
                set_digest = getattr(state.agent, '_hub_set_digest', None)
                if not callable(set_digest):
                    raise acp.RequestError(-32602, 'Unsupported session setting')
                if not isinstance(value, str) or len(value) > 12000:
                    raise acp.RequestError(-32602, 'Invalid development digest')
                set_digest(value)
            else:
                if value == 'default':
                    config = copy.deepcopy(state.agent._hub_default_reasoning)
                else:
                    config = parse_reasoning_effort(value)
                    if config is None:
                        raise acp.RequestError(-32602, 'Unsupported reasoning effort')
                state.agent.reasoning_config = config
            digest_injected = bool(getattr(state.agent, 'ephemeral_system_prompt', None))
        return SetSessionConfigOptionResponse(config_options=[], **{'_meta': {'agentHub': {
            'model': state.agent.model, 'reasoningConfig': copy.deepcopy(state.agent.reasoning_config),
            'toolCount': len(state.agent.tools), 'digest_injected': digest_injected
        }}})

    async def prompt(self, prompt, session_id, **kwargs):
        # ACP slash commands can mutate state independently of UI settings.
        if any(block.type == 'text' and block.text.lstrip().startswith('/') for block in prompt):
            raise acp.RequestError(-32602, 'Slash commands disabled; use session settings')
        state = await asyncio.to_thread(self.session_manager.get_session, session_id)
        if state is None:
            raise acp.RequestError(-32602, 'Unknown session')
        state.agent.tools = []
        state.agent.valid_tool_names = set()
        return await super().prompt(prompt=prompt, session_id=session_id, **kwargs)


# SessionManager normally discovers configured MCP processes before agent build.
# This chat-only child has no MCP tools, so it skips discovery in its own process.
import hermes_cli.mcp_startup
hermes_cli.mcp_startup.ensure_mcp_discovery_before_agent_build = lambda **kwargs: None
data.mkdir(parents=True, exist_ok=True)
# 会话库默认放同席自己的数据目录；给了 AGENT_HUB_HERMES_SESSION_DB 就用那份真实库
# （这样能看到并接着聊她自己的历史会话；不设变量即回到隔离行为）。
# 会话库优先级：命令行第 5 个参数 > 环境变量 > 同席自己的数据目录。
# 阿尔比恩那条走 WSL，Windows 的环境变量过不去 wsl.exe，所以库路径必须走命令行。
_explicit_db = sys.argv[4].strip() if len(sys.argv) > 4 and sys.argv[4].strip() else None
_session_db = Path(_explicit_db or os.environ.get('AGENT_HUB_HERMES_SESSION_DB') or (data / 'sessions.db')).resolve()
_sessions = ChatSessions(db=SessionDB(db_path=_session_db))
_sessions._relaxed_db()  # 在第一个 RPC 之前就放宽来源过滤（幂等）
agent = ChatACP(_sessions)
try:
    asyncio.run(acp.run_agent(agent, use_unstable_protocol=True))
finally:
    agent.session_manager.end_all_sessions()
