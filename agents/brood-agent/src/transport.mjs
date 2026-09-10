const MAX_EVENT_BYTES = 24 * 1024 * 1024;

export class ModelError extends Error {
  constructor(message, { status, code, retryable = false, retryAfter = 0 } = {}) {
    super(message);
    Object.assign(this, { status, code, retryable, retryAfter });
  }
}

export function parseSsePayloads(text) {
  return text.replace(/\r\n?/g, "\n").split("\n\n").flatMap(block => {
    const data = block.split("\n").filter(line => line.startsWith("data:"))
      .map(line => line.slice(5).replace(/^ /, "")).join("\n");
    return data && data !== "[DONE]" ? [JSON.parse(data)] : [];
  });
}

export function abortable(promise, signal) {
  if (!signal) return promise;
  signal.throwIfAborted();
  return new Promise((resolve, reject) => {
    const abort = () => reject(signal.reason ?? new Error("Turn cancelled"));
    signal.addEventListener("abort", abort, { once: true });
    promise.then(resolve, reject).finally(() => signal.removeEventListener("abort", abort));
  });
}

export async function waitForRetry(milliseconds, signal) {
  signal?.throwIfAborted();
  let timer;
  try { await abortable(new Promise(resolve => { timer = setTimeout(resolve, milliseconds); }), signal); }
  finally { clearTimeout(timer); }
}

async function boundedText(response, signal, maximum) {
  if (!response.body) throw new ModelError("Model response had no body", { retryable: true });
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let output = "";
  let bytes = 0;
  try {
    while (true) {
      const { value, done } = await abortable(reader.read(), signal);
      if (done) return output + decoder.decode();
      bytes += value.length;
      if (bytes > maximum) throw new ModelError("Model response exceeds size limit");
      output += decoder.decode(value, { stream: true });
    }
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
}

export async function readSse(response, { signal, idleTimeoutMs = 60000, onEvent = () => {} } = {}) {
  if (!response.body) throw new ModelError("Model response had no body", { retryable: true });
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let pending = "";
  let outputBytes = 0;
  const output = [];
  function consume(block) {
    for (const event of parseSsePayloads(block)) {
      if (event.type === "response.output_item.done" && event.item) {
        outputBytes += Buffer.byteLength(JSON.stringify(event.item));
        if (outputBytes > MAX_EVENT_BYTES) throw new ModelError("Model output exceeds size limit");
        output.push(event.item);
      }
      if (["response.failed", "response.incomplete", "error"].includes(event.type)) {
        const error = event.response?.error ?? event.error ?? event;
        const code = error.code ?? event.response?.incomplete_details?.reason;
        throw new ModelError(error.message ?? `Model response ${event.type}: ${code ?? "unknown error"}`, {
          code, retryable: ["server_error", "rate_limit_exceeded"].includes(code),
        });
      }
      onEvent(event);
      if (event.type === "response.completed") {
        const completed = event.response;
        if (!completed || (completed.status && completed.status !== "completed")) throw new ModelError("Invalid response.completed event");
        if (!completed.output?.length) completed.output = output;
        return completed;
      }
    }
  }
  try {
    while (true) {
      let timer;
      let packet;
      try {
        packet = await abortable(Promise.race([
          reader.read(),
          new Promise((_, reject) => { timer = setTimeout(() => reject(new ModelError("Model stream stalled", { retryable: true })), idleTimeoutMs); }),
        ]), signal);
      } finally { clearTimeout(timer); }
      pending += decoder.decode(packet.value ?? new Uint8Array(), { stream: !packet.done });
      // Match complete delimiters in the raw buffer so a CRLF split across reads
      // cannot become a spurious empty event.
      let match;
      while ((match = /\r\n\r\n|\n\n|\r\r/.exec(pending))) {
        const completed = consume(pending.slice(0, match.index));
        pending = pending.slice(match.index + match[0].length);
        if (completed) return completed;
      }
      if (Buffer.byteLength(pending) > MAX_EVENT_BYTES) throw new ModelError("Model SSE event exceeds size limit");
      if (packet.done) {
        const completed = consume(pending);
        if (completed) return completed;
        throw new ModelError("Model stream ended without response.completed", { retryable: true });
      }
    }
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
}

function redact(text, headers) {
  for (const [name, value] of Object.entries(headers ?? {})) {
    if (!/authorization|key|token|cookie/i.test(name)) continue;
    for (const secret of [value, value.replace(/^Bearer /i, "")]) if (secret.length > 3) text = text.replaceAll(secret, "[redacted]");
  }
  return text.slice(0, 4000);
}

export async function requestModel(fetchImpl, backend, body, {
  signal, settings = {}, retryBaseMs = 500, emit = () => {}, onEvent = () => {}, url = backend.url, json = false,
} = {}) {
  const retries = settings.max_retries ?? 3;
  let refreshed = false;
  for (let attempt = 0; attempt <= retries; attempt++) {
    signal?.throwIfAborted();
    const controller = new AbortController();
    const abort = () => controller.abort(signal.reason);
    signal?.addEventListener("abort", abort, { once: true });
    const timer = setTimeout(() => controller.abort(new ModelError("Model request timed out", { retryable: true })), settings.request_timeout_ms ?? 300000);
    let failure;
    try {
      onEvent({ type: "brood.attempt", attempt });
      const response = await abortable(fetchImpl(url, {
        method: "POST", headers: backend.headers, body: JSON.stringify(body), signal: controller.signal,
      }), controller.signal);
      if (!response.ok) {
        if (response.status === 401 && backend.refresh && !refreshed) {
          await response.body?.cancel();
          refreshed = true;
          await backend.refresh(controller.signal);
          attempt--;
          continue;
        }
        const text = redact(await boundedText(response, controller.signal, MAX_EVENT_BYTES), backend.headers);
        let error;
        try { error = JSON.parse(text).error; } catch {}
        const retryAfter = response.headers.get("retry-after");
        const delay = retryAfter && Number.isFinite(Number(retryAfter)) ? Number(retryAfter) * 1000 : Date.parse(retryAfter) - Date.now();
        throw new ModelError(`Model request failed (${response.status}): ${error?.message ?? text}`, {
          status: response.status, code: error?.code,
          retryable: [408, 409, 429].includes(response.status) || response.status >= 500,
          retryAfter: Number.isFinite(delay) ? Math.min(60000, Math.max(0, delay)) : 0,
        });
      }
      if (json || response.headers.get("content-type")?.includes("application/json")) {
        const completed = JSON.parse(await boundedText(response, controller.signal, MAX_EVENT_BYTES));
        if (completed.error || (completed.status && completed.status !== "completed")) throw new ModelError(completed.error?.message ?? `Model response ${completed.status}`, { code: completed.error?.code });
        return completed;
      }
      return await readSse(response, { signal: controller.signal, idleTimeoutMs: settings.stream_idle_timeout_ms, onEvent });
    } catch (error) {
      if (signal?.aborted) throw signal.reason;
      failure = controller.signal.aborted ? controller.signal.reason : error;
      if (!(failure instanceof ModelError)) failure = new ModelError(redact(failure.message, backend.headers), { retryable: true });
      if (!failure.retryable || attempt === retries) throw failure;
    } finally {
      clearTimeout(timer);
      signal?.removeEventListener("abort", abort);
    }
    emit("status", { text: `Model connection interrupted; retrying (${attempt + 1}/${retries})` });
    await waitForRetry(failure.retryAfter || Math.min(30000, retryBaseMs * 2 ** attempt), signal);
  }
}
