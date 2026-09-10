import { readFile, chmod } from "node:fs/promises";
import { homedir } from "node:os";
import { join, dirname } from "node:path";
import { createHash } from "node:crypto";
import { customBackend } from "./config.mjs";
import { atomicJson, ConversationStore } from "./storage.mjs";
import { abortable, waitForRetry } from "./transport.mjs";

const CODEX_CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_TOKEN_URL = "https://auth.openai.com/oauth/token";
const CODEX_RESPONSES_URL = "https://chatgpt.com/backend-api/codex/responses";
const OPENAI_RESPONSES_URL = "https://api.openai.com/v1/responses";

function decodeJwtPayload(token) {
  const payload = token.split(".")[1];
  if (!payload) throw new Error("access token is not a JWT");
  return JSON.parse(Buffer.from(payload, "base64url").toString("utf8"));
}

function codexAccountId(token, explicit) {
  if (explicit) return explicit;
  const payload = decodeJwtPayload(token);
  const account = payload?.["https://api.openai.com/auth"]?.chatgpt_account_id;
  if (!account) throw new Error("ChatGPT access token has no account id");
  return account;
}

function tokenExpiresSoon(token) {
  try {
    const expires = Number(decodeJwtPayload(token).exp) * 1000;
    return !Number.isFinite(expires) || expires < Date.now() + 60_000;
  } catch {
    return false;
  }
}

function broodAuthPath(env = process.env) {
  if (env.BROOD_AGENT_AUTH) return env.BROOD_AGENT_AUTH;
  const config = env.XDG_CONFIG_HOME || join(env.HOME || homedir(), ".config");
  return join(config, "brood", "agent-auth.json");
}

function codexAuthPath(env = process.env) {
  if (env.BROOD_AGENT_CODEX_AUTH) return env.BROOD_AGENT_CODEX_AUTH;
  return join(env.HOME || homedir(), ".codex", "auth.json");
}

async function readJson(path) {
  try {
    return JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    if (error?.code === "ENOENT") return undefined;
    throw error;
  }
}

async function saveBroodAuth(auth, path = broodAuthPath()) {
  await atomicJson(path, auth);
}

async function refreshCodexAuth(auth, path, fetchImpl, signal) {
  const response = await abortable(fetchImpl(CODEX_TOKEN_URL, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "refresh_token",
      refresh_token: auth.refresh_token,
      client_id: CODEX_CLIENT_ID,
    }),
    signal,
  }), signal);
  if (!response.ok) {
    throw new Error(`Refresh Brood agent login: HTTP ${response.status}. Run brood-agent auth import-codex to import a current Codex login, or configure an API-key provider.`);
  }
  const tokens = await abortable(response.json(), signal);
  if (!tokens.access_token || !tokens.refresh_token) {
    throw new Error("refresh Brood agent login: token response was incomplete");
  }
  const refreshed = {
    provider: "openai-codex",
    access_token: tokens.access_token,
    refresh_token: tokens.refresh_token,
    account_id: codexAccountId(
      tokens.access_token,
      tokens.account_id ?? auth.account_id,
    ),
  };
  await saveBroodAuth(refreshed, path);
  return refreshed;
}

export async function resolveModelBackend(env = process.env, fetchImpl = fetch, settings = { provider: "auto" }, signal) {
  signal?.throwIfAborted();
  const custom = customBackend(settings, env);
  if (custom) return custom;
  if (settings.provider === "openai" && !env.OPENAI_API_KEY) throw new Error("Provider openai requires OPENAI_API_KEY");
  if (env.OPENAI_API_KEY && settings.provider !== "openai-codex") {
    return {
      kind: "openai-responses",
      url: env.BROOD_AGENT_MODEL_URL || OPENAI_RESPONSES_URL,
      headers: {
        authorization: `Bearer ${env.OPENAI_API_KEY}`,
        "content-type": "application/json",
        accept: "text/event-stream",
      },
    };
  }

  let auth = await loadCodexAuth(env, fetchImpl, signal);
  const backend = {
    kind: "openai-codex",
    url: env.BROOD_AGENT_MODEL_URL || CODEX_RESPONSES_URL,
    headers: {
      authorization: `Bearer ${auth.access_token}`,
      "chatgpt-account-id": codexAccountId(auth.access_token, auth.account_id),
      originator: "brood",
      "user-agent": "brood-agent/0.1.0",
      "openai-beta": "responses=experimental",
      "content-type": "application/json",
      accept: "text/event-stream",
    },
    async refresh(refreshSignal) {
      auth = await loadCodexAuth(env, fetchImpl, refreshSignal, auth.access_token);
      backend.headers.authorization = `Bearer ${auth.access_token}`;
      backend.headers["chatgpt-account-id"] = codexAccountId(auth.access_token, auth.account_id);
    },
  };
  return backend;
}

export async function importCodexLogin(env = process.env, fetchImpl = fetch, signal) {
  await loadCodexAuth(env, fetchImpl, signal, undefined, true);
  return broodAuthPath(env);
}

async function loadCodexAuth(env, fetchImpl, parentSignal, rejectedToken, importNew = false) {
  const timeout = AbortSignal.timeout(30000);
  const signal = parentSignal ? AbortSignal.any([parentSignal, timeout]) : timeout;
  const authPath = broodAuthPath(env);
  const lock = new ConversationStore(dirname(authPath), `auth-${createHash("sha256").update(authPath).digest("hex").slice(0, 24)}`);
  while (true) {
    signal.throwIfAborted();
    try { await lock.acquire(); break; }
    catch (error) { if (error.code !== "CONVERSATION_BUSY") throw error; await waitForRetry(50, signal); }
  }
  try {
    let auth = importNew ? undefined : await readJson(authPath);
    if (!auth) {
      const imported = await readJson(codexAuthPath(env));
      if (imported?.tokens?.access_token && imported.tokens.refresh_token) {
        auth = {
          provider: "openai-codex",
          access_token: imported.tokens.access_token,
          refresh_token: imported.tokens.refresh_token,
          account_id: imported.tokens.account_id,
        };
      }
    }
    if (!auth?.access_token) {
      throw new Error(
        "No Brood agent model login. Set OPENAI_API_KEY or provide Brood agent OAuth credentials.",
      );
    }
    if (tokenExpiresSoon(auth.access_token) || auth.access_token === rejectedToken) {
      if (!auth.refresh_token) throw new Error("Brood agent model login expired");
      auth = await refreshCodexAuth(auth, authPath, fetchImpl, signal);
    }
    await saveBroodAuth(auth, authPath);
    await chmod(authPath, 0o600);
    return auth;
  } finally { await lock.close(); }
}
