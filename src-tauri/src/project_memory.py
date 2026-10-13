"""Project memory on the installed OpenViking client: a shared project pool plus each member's private
notes, with explicit sharing. Access rules are enforced here; no credential is ever printed."""
import asyncio
import contextlib
import io
import json
import re
import sys

AGENTS = ('codex-win', 'hermes-win', 'dsh-win', 'claude-win')
NAME = re.compile(r'(?!\.)[\w.-]{1,80}')
SECRET = re.compile(r'-----BEGIN .*PRIVATE KEY-----|\bsk-[A-Za-z0-9_-]{20,}|\bghp_[A-Za-z0-9]{20,}|\bAKIA[0-9A-Z]{16}\b')


class Refused(Exception):
    """A rule violation the agent can fix; its message is shown verbatim."""


def note_name(value):
    if not isinstance(value, str) or not NAME.fullmatch(value) or value.endswith('.md'):
        raise Refused('笔记名只能用字母、数字、中文、下划线、点和连字符（1-80 字），不要带 .md')
    return value


def body(project, agent, name, scope, content, shared_from=None):
    if not isinstance(content, str) or not content.strip() or len(content) > 16000:
        raise Refused('笔记内容不能为空，且不超过 16000 字')
    if SECRET.search(content):
        raise Refused('笔记内容疑似包含凭据，已拒绝保存')
    header = [f'# {name}', '', f'Project: {project}', f'Author: {agent}', f'Scope: {scope}']
    if shared_from:
        header.append(f'Shared-From: {shared_from}')
    return '\n'.join(header) + '\n\n' + content


def missing(error):
    """只有“不存在”才当作空；认证、连接等错误必须报出来，不能静默变成空结果。"""
    text = f'{type(error).__name__} {error}'.lower()
    return 'notfound' in text or 'not found' in text or 'no such' in text or '404' in text


def author_of(text):
    match = re.search(r'^Author: (\S+)$', text or '', re.M)
    return match.group(1) if match else None


async def run(payload):
    from openviking_cli.client._http_compat import AsyncHTTPClient
    project, agent, tool, args = (payload[key] for key in ('project', 'agent', 'tool', 'args'))
    if not re.fullmatch(r'[A-Za-z0-9_-]{1,80}', project) or agent not in AGENTS:
        raise Refused('项目或成员身份无效')
    root = f'viking://resources/tongxi/projects/{project}/'
    shared = root + 'shared/'
    private = root + f'private/{agent}/'
    legacy = root + f'{agent}/'  # 旧版按成员分目录的笔记，归入该成员的私有区

    def readable(uri):
        return isinstance(uri, str) and any(uri.startswith(prefix) and NAME.fullmatch(uri[len(prefix):-3] or '-')
                                            and uri.endswith('.md') for prefix in (shared, private, legacy))

    def scopes(value):
        value = value or 'all'
        if value not in ('all', 'shared', 'private'):
            raise Refused('scope 只能是 all、shared 或 private')
        return {'all': [shared, private, legacy], 'shared': [shared], 'private': [private, legacy]}[value]

    client = AsyncHTTPClient(timeout=30)
    try:
        await client.initialize()

        async def existing(uri):
            try:
                return await client.read(uri)
            except Exception as error:  # noqa: BLE001
                if missing(error):
                    return None
                raise

        async def put_shared(name, text):
            uri = shared + name + '.md'
            current = await existing(uri)
            if current is not None and author_of(current) != agent:
                raise Refused(f'共享区已有 {author_of(current) or "其他成员"} 写的同名笔记，请换一个 name；共享笔记只由作者本人修改')
            await client.write(uri, text, mode='replace', wait=False)
            return uri

        if tool == 'hub_memory_write':
            name = note_name(args.get('name'))
            scope = args.get('scope') or 'private'
            if scope not in ('private', 'shared'):
                raise Refused('scope 只能是 private 或 shared')
            text = body(project, agent, name, scope, args.get('content'))
            if scope == 'shared':
                uri = await put_shared(name, text)
            else:
                uri = private + name + '.md'
                await client.write(uri, text, mode='replace', wait=False)
            return {'uri': uri, 'scope': scope, 'stored': True, 'indexing': 'background'}

        if tool == 'hub_memory_read':
            uri = args.get('uri')
            if not readable(uri):
                raise Refused('只能读取本项目的共享笔记或你自己的私有笔记')
            return {'uri': uri, 'content': await client.read(uri)}

        if tool == 'hub_memory_search':
            query = args.get('query')
            if not isinstance(query, str) or not query.strip():
                raise Refused('请给出检索内容')
            allowed = scopes(args.get('scope'))
            result = await client.find(query, target_uri=root, limit=24)
            notes = [note for note in result.get('resources', [])
                     if any(note.get('uri', '').startswith(prefix) for prefix in allowed)][:8]
            for note in notes:
                note['scope'] = 'shared' if note['uri'].startswith(shared) else 'private'
            return {'shared_uri': shared, 'private_uri': private, 'notes': notes}

        if tool == 'hub_memory_list':
            entries = []
            for prefix in scopes(args.get('scope')):
                try:
                    listed = await client.ls(prefix)
                except Exception as error:  # noqa: BLE001
                    if missing(error):
                        continue
                    raise
                items = listed if isinstance(listed, list) else listed.get('entries', listed.get('children', []))
                for item in items:
                    uri = item.get('uri') if isinstance(item, dict) else str(item)
                    if uri and not uri.startswith('viking://'):
                        uri = prefix + uri.lstrip('/')
                    if uri and uri.endswith('.md'):
                        entries.append({'uri': uri, 'scope': 'shared' if prefix == shared else 'private'})
            return {'shared_uri': shared, 'private_uri': private, 'notes': entries[:200]}

        if tool == 'hub_memory_share':
            source = args.get('uri')
            if not readable(source) or source.startswith(shared):
                raise Refused('只能分享你自己的私有笔记（private 区的 URI）')
            original = await client.read(source)
            name = note_name(args.get('name') or source.rsplit('/', 1)[-1][:-3])
            content = re.sub(r'\A# .*?\n\n(?:[A-Za-z-]+: .*\n)*\n', '', original, count=1, flags=re.S) or original
            note = args.get('note')
            if note:
                content = f'{note.strip()}\n\n{content}'
            uri = await put_shared(name, body(project, agent, name, 'shared', content, shared_from=source))
            return {'uri': uri, 'shared_from': source, 'stored': True, 'private_kept': True}

        raise Refused('不支持的项目记忆工具')
    finally:
        await client.close()


def failure(error):
    """Fixed categories only: upstream messages may carry configuration details."""
    if isinstance(error, Refused):
        return str(error)
    text = f'{type(error).__name__} {error}'.lower()
    if 'module' in text and ('no module' in text or 'import' in text):
        return '项目记忆客户端未安装：AGENT_HUB_OPENVIKING_PYTHON 指向的 Python 里没有 OpenViking 客户端；不影响项目继续执行。'
    if any(marker in text for marker in ('401', '403', 'unauthorized', 'forbidden', 'api key', 'api_key', 'apikey')):
        return '项目记忆认证失败：请检查同席使用的 OpenViking 配置（默认 ~/.openviking/ovcli.tooling.conf，可用 AGENT_HUB_OPENVIKING_CONFIG 指定）里的 api_key；不影响项目继续执行。'
    if any(marker in text for marker in ('connect', 'refused', 'timeout', 'timed out', 'unreachable')):
        return '项目记忆服务不可达：请确认本机 OpenViking 服务已启动；不影响项目继续执行。'
    return '项目记忆操作未完成（OpenViking 返回错误）；不影响项目继续执行。'


try:
    payload = json.load(sys.stdin)
    with contextlib.redirect_stdout(io.StringIO()):
        result = asyncio.run(run(payload))
    print(json.dumps({'result': result}, ensure_ascii=False))
except Exception as error:  # noqa: BLE001
    print(json.dumps({'error': failure(error)}, ensure_ascii=False))
