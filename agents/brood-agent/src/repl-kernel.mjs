import repl from "node:repl";
import { PassThrough } from "node:stream";
import { inspect } from "node:util";
import { AsyncLocalStorage } from "node:async_hooks";

const context = new AsyncLocalStorage();
const pending = new Map();
let nextTool = 0;
let active;
const server = repl.start({ input: new PassThrough(), output: new PassThrough(), terminal: false, useGlobal: false });
const send = message => process.send?.(message);
function write(...values) {
  if (context.getStore() !== active) return;
  send({ type: "output", id: active, text: values.map(value => typeof value === "string" ? value : inspect(value, { depth: 5, maxArrayLength: 100, maxStringLength: 32768 })).join(" ") + "\n" });
}
Object.assign(server.context, {
  console: Object.fromEntries(["log", "info", "warn", "error", "debug", "dir"].map(name => [name, write])),
  text: write,
  tools: new Proxy({}, {
    get(_, name) {
      if (name === "then" || typeof name !== "string") return undefined;
      return args => {
        const id = context.getStore();
        if (!active || id !== active) return Promise.reject(new Error("Tool calls must be awaited within the current REPL cell"));
        const call = ++nextTool;
        return new Promise((resolve, reject) => {
          pending.set(call, { resolve, reject });
          send({ type: "tool", id, call, name, args: args ?? {} });
        });
      };
    },
  }),
});

process.on("message", message => {
  if (message.type === "tool_result") {
    const promise = pending.get(message.call);
    pending.delete(message.call);
    if (message.error) promise?.reject(new Error(message.error));
    else promise?.resolve(message.result);
  } else if (message.type === "eval") {
    active = message.id;
    context.run(active, () => server.eval(message.code, server.context, "brood-repl", (error, result) => {
      if (!error && result !== undefined) write(result);
      // Unawaited tool calls cannot escape the lifetime of their cell.
      if (pending.size) error ??= new Error("Await all tools before completing the cell");
      send({ type: "done", id: message.id, error: error?.message, reset: pending.size > 0 });
      active = undefined;
    }));
  }
});
// Default REPL evaluation emits some errors through its domain instead of the
// eval callback. Convert that path into a normal cell failure as well.
server._domain.on("error", error => {
  if (active && context.getStore() === active) {
    send({ type: "done", id: active, error: error.message, reset: pending.size > 0 });
    active = undefined;
  }
});
process.on("disconnect", () => process.exit(0));
