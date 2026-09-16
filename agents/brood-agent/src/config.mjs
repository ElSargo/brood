import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

export const BUILTIN_PROVIDERS = Object.freeze(["auto", "openai", "openai-codex", "opencode-go"]);

export const DEFAULTS = Object.freeze({
  model: "gpt-5.6-sol",
  reasoning: "medium",
  provider: "auto",
  context_window: 128000,
  compact_threshold: 90000,
  max_steps: 256,
  tool_output_bytes: 32768,
  request_timeout_ms: 300000,
  stream_idle_timeout_ms: 60000,
  max_retries: 3,
  compaction: "auto",
});

export function configPath(env = process.env) {
  return env.BROOD_AGENT_CONFIG || join(env.XDG_CONFIG_HOME || join(env.HOME || homedir(), ".config"), "brood", "agent.json");
}

function object(value, name) {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error(`${name} must be an object`);
}

const LIMITS = {
  context_window: [1024, 10000000], compact_threshold: [256, 10000000],
  max_steps: [1, 10000], tool_output_bytes: [1024, 1048576],
  request_timeout_ms: [100, 3600000], stream_idle_timeout_ms: [100, 3600000], max_retries: [0, 10],
};
const SETTINGS = new Set([...Object.keys(DEFAULTS), "profile", "providers", "profiles"]);

function validateSettings(value, name, allowed = SETTINGS) {
  object(value, name);
  for (const [key, entry] of Object.entries(value)) {
    if (!allowed.has(key)) throw new Error(`Unknown ${name} setting: ${key}`);
    if (LIMITS[key]) {
      const [min, max] = LIMITS[key];
      if (!Number.isSafeInteger(entry) || entry < min || entry > max) throw new Error(`${name}.${key} must be an integer between ${min} and ${max}`);
    } else if (["provider", "profile", "model", "reasoning", "compaction"].includes(key)) {
      if (typeof entry !== "string" || !entry.trim()) throw new Error(`${name}.${key} must be a nonempty string`);
    }
  }
  if (value.compaction && !["auto", "remote", "summary", "off"].includes(value.compaction)) throw new Error(`${name}.compaction must be auto, remote, summary, or off`);
  if (value.reasoning && !["default", "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"].includes(value.reasoning)) throw new Error(`${name}.reasoning is invalid`);
}

export function validateConfig(config) {
  validateSettings(config, "agent config");
  for (const [name, profile] of Object.entries(config.profiles ?? {})) {
    validateSettings(profile, `profile ${name}`, new Set(Object.keys(DEFAULTS)));
  }
  if (config.profiles !== undefined) object(config.profiles, "profiles");
  if (config.providers !== undefined) object(config.providers, "providers");
  for (const [id, provider] of Object.entries(config.providers ?? {})) {
    object(provider, `provider ${id}`);
    if (BUILTIN_PROVIDERS.includes(id)) throw new Error(`Provider id ${id} is reserved`);
    const keys = new Set(["base_url", "env_key", "http_headers", "env_http_headers", "query_params", "wire_api", "supports_reasoning", "supports_verbosity", "compaction"]);
    for (const key of Object.keys(provider)) if (!keys.has(key)) throw new Error(`Unknown provider ${id} setting: ${key}`);
    let url;
    try { url = new URL(provider.base_url); } catch { throw new Error(`Provider ${id} needs a valid base_url`); }
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.hash || url.search) throw new Error(`Provider ${id} base_url must be an HTTP(S) URL without credentials, query, or fragment`);
    if (provider.wire_api && provider.wire_api !== "responses") throw new Error(`Provider ${id}: only the Responses wire API is supported`);
    if (provider.env_key !== undefined && !/^[A-Za-z_][A-Za-z0-9_]*$/.test(provider.env_key)) throw new Error(`Provider ${id} env_key must name an environment variable`);
    for (const key of ["supports_reasoning", "supports_verbosity"]) if (provider[key] !== undefined && typeof provider[key] !== "boolean") throw new Error(`Provider ${id}.${key} must be a boolean`);
    if (provider.compaction && !["auto", "remote", "summary", "off"].includes(provider.compaction)) throw new Error(`Provider ${id}.compaction is invalid`);
    for (const key of ["http_headers", "env_http_headers", "query_params"]) {
      if (provider[key] === undefined) continue;
      object(provider[key], `provider ${id}.${key}`);
      for (const [header, value] of Object.entries(provider[key])) {
        if (typeof value !== "string" || /[\r\n]/.test(header + value)) throw new Error(`Provider ${id}.${key} must contain single-line string values`);
        if (key === "http_headers" && /authorization|api[-_]?key|cookie/i.test(header)) throw new Error(`Provider ${id}: credentials belong in env_key or env_http_headers`);
      }
    }
  }
  return config;
}

export async function loadConfig(env = process.env) {
  const path = configPath(env);
  try { return validateConfig(JSON.parse(await readFile(path, "utf8"))); }
  catch (error) {
    if (error.code === "ENOENT" && !env.BROOD_AGENT_CONFIG) return {};
    throw new Error(`Read ${path}: ${error.message}`);
  }
}

export function resolveSettings(config, request = {}, env = process.env) {
  validateConfig(config);
  const profileName = request.profile || env.BROOD_AGENT_PROFILE || config.profile;
  const profile = profileName ? config.profiles?.[profileName] : {};
  if (profileName && !profile) throw new Error(`Unknown agent profile: ${profileName}`);
  const settings = { ...DEFAULTS, ...config, ...profile };
  for (const key of Object.keys(DEFAULTS)) {
    if (request[key] !== undefined && request[key] !== "" && request[key] !== "default") settings[key] = request[key];
  }
  settings.provider = request.provider || env.BROOD_AGENT_PROVIDER || settings.provider;
  if (settings.provider === "default") settings.provider = config.provider || DEFAULTS.provider;
  validateSettings(settings, "resolved config");
  if (settings.compact_threshold >= settings.context_window) throw new Error("compact_threshold must be below context_window");
  if (!BUILTIN_PROVIDERS.includes(settings.provider) && !config.providers?.[settings.provider]) throw new Error(`Unknown agent provider: ${settings.provider}`);
  return settings;
}

export function customBackend(settings, env = process.env) {
  const provider = settings.providers?.[settings.provider];
  if (!provider) return undefined;
  const headers = { "content-type": "application/json", accept: "text/event-stream", ...provider.http_headers };
  if (provider.env_key) {
    if (!env[provider.env_key]) throw new Error(`Provider ${settings.provider} requires ${provider.env_key}`);
    headers.authorization = `Bearer ${env[provider.env_key]}`;
  }
  for (const [header, name] of Object.entries(provider.env_http_headers ?? {})) {
    if (!env[name]) throw new Error(`Provider ${settings.provider} requires ${name}`);
    headers[header] = env[name];
  }
  const url = new URL(`${provider.base_url.replace(/\/+$/, "")}/responses`);
  for (const [name, value] of Object.entries(provider.query_params ?? {})) url.searchParams.set(name, value);
  return { kind: settings.provider, url: url.href, headers, supportsReasoning: provider.supports_reasoning !== false, supportsVerbosity: provider.supports_verbosity !== false, compaction: provider.compaction ?? settings.compaction };
}
