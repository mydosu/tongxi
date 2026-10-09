"""Project-scoped access to the installed OpenViking client; no credential output."""
import asyncio
import contextlib
import io
import json
import re
import sys

async def run(payload):
    from openviking_cli.client._http_compat import AsyncHTTPClient
    project, agent, tool, args = (payload[key] for key in ('project', 'agent', 'tool', 'args'))
    if not re.fullmatch(r'[A-Za-z0-9_-]{1,80}', project) or agent not in ('codex-win', 'hermes-win', 'dsh-win'):
        raise ValueError('invalid scope')
    root = f'viking://resources/tongxi/projects/{project}/'
    client = AsyncHTTPClient(timeout=30)
    try:
        await client.initialize()
        if tool == 'hub_memory_write':
            if not re.fullmatch(r'[A-Za-z0-9_-]{1,80}', args['name']): raise ValueError('invalid note name')
            content = args['content']
            if not content.strip() or len(content)>16000 or re.search(r'-----BEGIN .*PRIVATE KEY-----|\bsk-[A-Za-z0-9_-]{20,}', content): raise ValueError('invalid note content')
            uri=root+agent+'/'+args['name']+'.md'
            await client.write(uri, f'# {args["name"]}\n\nProject: {project}\nAuthor: {agent}\n\n{content}', mode='replace', wait=False)
            return {'uri':uri,'stored':True,'project_uri':root,'indexing':'background'}
        if tool == 'hub_memory_read':
            uri=args['uri']
            if not uri.startswith(root) or not re.fullmatch(r'(codex-win|hermes-win|dsh-win)/[A-Za-z0-9_-]{1,80}\.md', uri[len(root):]): raise ValueError('outside project')
            return {'uri':uri,'content':await client.read(uri)}
        if tool == 'hub_memory_search':
            result=await client.find(args['query'], target_uri=root, limit=8)
            notes=[note for note in result.get('resources',[]) if note.get('uri','').startswith(root)]
            return {'project_uri':root,'notes':notes}
        raise ValueError('unsupported tool')
    finally:
        await client.close()

try:
    payload=json.load(sys.stdin)
    with contextlib.redirect_stdout(io.StringIO()): result=asyncio.run(run(payload))
    print(json.dumps({'result':result},ensure_ascii=False))
except Exception:
    print(json.dumps({'error':'项目记忆操作未完成，请检查本机 OpenViking 服务与客户端；不影响项目继续执行。'},ensure_ascii=False))
