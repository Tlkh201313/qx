import { spawn } from "node:child_process";

const PROTOCOL = "2024-11-05";

/**
 * A stdio MCP client for qx, so a pi extension can expose qx's tools natively.
 *
 * One child per bridge, started on the first call and respawned after it dies.
 * Newline-delimited JSON-RPC with an id map; an aborted call sends
 * `notifications/cancelled` so qx stops the command instead of running it out.
 */
export class QxBridge {
  constructor(exe, env = {}) {
    this.exe = exe;
    this.env = env;
    this.child = null;
    this.nextId = 1;
    this.pending = new Map();
    this.buffer = "";
    this.ready = null;
  }

  start() {
    if (this.ready) return this.ready;
    const child = spawn(this.exe, ["mcp"], {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, QX_CLIENT: "pi", ...this.env },
      windowsHide: true,
    });
    this.child = child;
    this.buffer = "";
    child.stdout.setEncoding("utf8");
    // Events are scoped to their own child: a dying old process must not fail its replacement.
    child.stdout.on("data", (chunk) => {
      if (this.child === child) this.onData(chunk);
    });
    // qx writes nothing to stderr on the happy path; never let a full pipe block it.
    child.stderr.on("data", () => {});
    child.stdout.on("error", () => {});
    child.stderr.on("error", () => {});
    // A write to a dead child (EPIPE) is an 'error' event; unhandled it would crash pi.
    child.stdin.on("error", (error) => this.failFrom(child, error));
    child.on("error", (error) => this.failFrom(child, error));
    child.on("exit", () => this.failFrom(child, new Error("qx exited")));
    this.ready = this.request("initialize", {
      protocolVersion: PROTOCOL,
      capabilities: {},
      clientInfo: { name: "pi", version: "qx-bridge" },
    })
      .then(() => {
        this.notify("notifications/initialized", {});
      })
      .catch((error) => {
        this.ready = null;
        throw error;
      });
    return this.ready;
  }

  onData(chunk) {
    this.buffer += chunk;
    let index = this.buffer.indexOf("\n");
    while (index >= 0) {
      const line = this.buffer.slice(0, index).trim();
      this.buffer = this.buffer.slice(index + 1);
      index = this.buffer.indexOf("\n");
      if (!line) continue;
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        continue;
      }
      const entry = this.pending.get(message.id);
      if (!entry) continue;
      this.pending.delete(message.id);
      if (message.error) {
        entry.reject(new Error(message.error.message ?? JSON.stringify(message.error)));
      } else {
        entry.resolve(message.result);
      }
    }
  }

  failFrom(child, error) {
    if (this.child === child) this.fail(error);
  }

  fail(error) {
    for (const entry of this.pending.values()) entry.reject(error);
    this.pending.clear();
    this.child = null;
    this.ready = null;
    // Half a line from the dead child would be glued to the next child's first reply.
    this.buffer = "";
  }

  send(payload) {
    const child = this.child;
    if (!child || child.killed || !child.stdin.writable) {
      throw new Error("qx bridge is not running");
    }
    child.stdin.write(`${JSON.stringify(payload)}\n`);
  }

  notify(method, params) {
    try {
      this.send({ jsonrpc: "2.0", method, params });
    } catch {
      // The child is gone; the next call respawns it.
    }
  }

  request(method, params, signal) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      try {
        this.send({ jsonrpc: "2.0", id, method, params });
      } catch (error) {
        this.pending.delete(id);
        reject(error);
        return;
      }
      if (!signal) return;
      const onAbort = () => {
        this.notify("notifications/cancelled", { requestId: id, reason: "aborted" });
        const entry = this.pending.get(id);
        if (entry) {
          this.pending.delete(id);
          entry.reject(new Error("aborted"));
        }
      };
      if (signal.aborted) onAbort();
      else signal.addEventListener("abort", onAbort, { once: true });
    });
  }

  async call(tool, args, signal) {
    await this.start();
    const result = await this.request("tools/call", { name: tool, arguments: args }, signal);
    const content = Array.isArray(result?.content) ? result.content : [];
    const text = content
      .filter((part) => part?.type === "text")
      .map((part) => part.text)
      .join("\n");
    return {
      text,
      isError: result?.isError === true,
      structured: result?.structuredContent,
    };
  }

  close() {
    const child = this.child;
    this.child = null;
    this.ready = null;
    if (!child) return;
    try {
      child.stdin.end();
    } catch {
      // already closed
    }
    try {
      child.kill();
    } catch {
      // already dead
    }
  }
}
