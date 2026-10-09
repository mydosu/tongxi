import assert from 'node:assert/strict';
import { dailyFallback, installDailyFallback } from '../src-tauri/src/dsh_fallback.mjs';
const stream = async function* (...chunks) { yield* chunks; };
const fail = { type: 'finish', reason: { kind: 'error', failure: { code: 'RATE_LIMIT', status: 429, message: 'PRIVATE_DATA' } } };
const done = { type: 'finish', reason: { kind: 'stop' } };
const body = { type: 'text-delta', index: 0, text: 'ok' };
let backup=0, notices=0;
const collect = async input => { const rows=[]; for await (const chunk of input) rows.push(chunk); return rows; };
const secondary=()=>{ backup++; return stream(body,done); };
assert.deepEqual(await collect(dailyFallback({},()=>stream(fail),secondary,()=>notices++)),[body,done]);
assert.equal(backup,1);assert.equal(notices,1);
assert.deepEqual(await collect(dailyFallback({},()=>stream(body,fail),secondary,()=>notices++)),[body,fail]);
assert.equal(backup,1);
for (const code of ['CONTEXT_WINDOW_EXCEEDED','BAD_REQUEST','INVALID_TOOL']) {
 const failure={...fail,reason:{kind:'error',failure:{code}}};
 assert.deepEqual(await collect(dailyFallback({},()=>stream(failure),secondary,()=>notices++)),[failure]);
}
const signal=AbortSignal.abort();await collect(dailyFallback({signal},()=>stream(fail),secondary,()=>notices++));assert.equal(backup,1);
let hook;const requests=[],routeNotices=[];
const ctx={on(name,callback){assert.equal(name,'llm/stream');hook=callback;},llm:{stream(options){requests.push(options);return hook(options,()=>stream(body,done));}}};
installDailyFallback(ctx,[{id:'command-code-daily',model:'ds-v4.1'},{id:'command-code-daily-2',model:'ds-v4.1'}],notice=>routeNotices.push(notice));
const request={provider:'command-code-daily',model:'ds-v4.1',sessionId:'same-session',signal:new AbortController().signal,messages:[{content:'completed tool result'}]};
await collect(hook(request,()=>stream(fail)));await collect(hook(request,()=>stream(fail)));
assert.equal(requests.length,2);assert.ok(requests.every(item=>item.provider==='command-code-daily-2'&&item.messages===request.messages));
assert.equal(routeNotices.filter(item=>item.fallback).length,1);
console.log('PASS fallback: quota/rate route switch, no partial replay, no context/tool retry, cancellation, same-turn pinning and history retained');
