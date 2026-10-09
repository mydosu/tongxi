const retryCodes = new Set(['QUOTA', 'RATE_LIMIT', 'SERVER', 'TIMEOUT', 'TRANSPORT', 'EMPTY_RESPONSE', 'INVALID_CREDENTIAL']);
export function retryableRouteFailure(chunk) {
  const failure = chunk?.reason?.failure;
  return chunk?.type === 'finish' && chunk.reason?.kind === 'error' && (retryCodes.has(failure?.code) || [401, 402, 408, 429, 500, 502, 503, 504].includes(failure?.status));
}

// Retry only a model generation that has not emitted usable content. The agent
// loop and its already executed tools stay in place; the whole prompt is never replayed.
export async function* dailyFallback(options, next, backup, notice, primaryNotice = () => {}) {
  const prefix = [];
  let emitted = false;
  for await (const chunk of next()) {
    if (!emitted && retryableRouteFailure(chunk) && !options.signal?.aborted) {
      notice();
      yield* backup();
      return;
    }
    if (!emitted && (chunk.type === 'block-start' || chunk.type === 'usage')) { prefix.push(chunk); continue; }
    if (!emitted && chunk.type !== 'finish') primaryNotice();
    yield* prefix.splice(0);
    yield chunk;
    if (chunk.type !== 'finish') emitted = true;
  }
  yield* prefix;
}

export function installDailyFallback(ctx, routes, notify) {
  const primary = routes.find(route => route.id === 'command-code-daily');
  const fallback = routes.find(route => route.id === 'command-code-daily-2');
  if (!primary || !fallback) throw new Error('DSH fallback routes unavailable');
  const pins = new WeakMap();
  ctx.on('llm/stream', (options, next) => {
    const session = String(options.sessionId || '');
    const pinned = options.signal && pins.get(options.signal)?.has(session);
    const route = options.provider === primary.id && pinned ? fallback : routes.find(route => route.id === options.provider);
    const routed = route => Object.freeze({ ...options, provider: route.id, model: route.model });
    const announce = (route, switched) => notify({ sessionId: session, modelId: JSON.stringify([route.id, route.model]), fallback: switched });
    if (options.provider !== primary.id) {
      if (route) announce(route, false);
      return next();
    }
    if (pinned) return ctx.llm.stream(routed(fallback));
    return dailyFallback(options, next, () => ctx.llm.stream(routed(fallback)), () => {
      if (options.signal) {
        if (!pins.has(options.signal)) pins.set(options.signal, new Set());
        pins.get(options.signal).add(session);
      }
      announce(fallback, true);
    }, () => announce(primary, false));
  });
}
