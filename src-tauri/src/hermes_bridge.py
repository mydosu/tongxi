"""In-process extension of installed Hermes ACP; no changes to the installation.

Stdio contains protocol only. Native auth remains owned by Hermes. Histories
use the hub's own SessionDB; native Hermes tools and MCP remain enabled by their normal configuration.
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
        coordinator = os.environ.get('AGENT_HUB_COORDINATOR_MODE') == '1'
        if coordinator:
            kwargs['enabled_toolsets'] = []
            kwargs['disabled_toolsets'] = []
        agent = super()._make_agent(**kwargs)
        if coordinator:
            import types
            agent.tools = []
            def reject_tools(_self, *_args, **_kwargs):
                raise RuntimeError('Coordinator has no project tool permissions')
            agent._execute_tool_calls = types.MethodType(reject_tools, agent)
        agent._hub_default_reasoning = copy.deepcopy(agent.reasoning_config)
        # 思考块只从流式增量推给 ACP 客户端：profile 里 model.streaming=false 的会话
        # （例如阿尔比恩）非流式请求，Hermes 的非流式兜底又要求「未设 stream_delta_callback」，
        # 结果推理只落库、发不出来。这里只把本会话恢复成流式，不动 profile、不影响其他链路。
        agent._disable_streaming = False
        return agent


class ChatACP(HermesACPAgent):
    def _switch_model(self, state, raw_model, *, keep_endpoint=False):
        result = super()._switch_model(state, raw_model, keep_endpoint=keep_endpoint)
        # Keep the provider-qualified identity resolved by Hermes, not its bare API model name.
        from acp_adapter.model_catalog import encode_model_choice
        state.agent._hub_model_choice = encode_model_choice(result[1], result[2])
        return result

    async def close_session(self, session_id, **kwargs):
        """把会话移交给 CLI 时卸下内存里的这一份；下次载入从库里重读，接上 CLI 里的新对话。"""
        manager = self.session_manager
        with manager._lock:
            state = manager._sessions.get(session_id)
            if state is not None and not state.is_running:
                manager._sessions.pop(session_id, None)
        return None

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
            'model': state.agent.model, 'modelId': getattr(state.agent, '_hub_model_choice', None), 'reasoningConfig': copy.deepcopy(state.agent.reasoning_config),
            'toolCount': len(state.agent.tools), 'digest_injected': digest_injected
        }}})

# Keep Hermes' normal tools, MCP servers, slash commands, and configured permissions.
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
