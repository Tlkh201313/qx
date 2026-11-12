// Run: node integrations/pi/bridge.test.mjs   (no qx binary needed)
import assert from "node:assert/strict";
import { getEventListeners } from "node:events";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { QxBridge } from "./qx/bridge.js";

const within = (ms, promise, what) =>
  Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error(`timed out: ${what}`)), ms)),
  ]);

// An exception that escapes the bridge is what would crash pi: report it and fail the run.
let uncaught = null;
process.on("uncaughtException", (error) => {
  uncaught = uncaught ?? error;
  console.error("FAIL:", error?.stack ?? error);
  process.exitCode = 1;
});

// 1. A binary that cannot start rejects the call cleanly; it never throws out of the bridge.
{
  const bridge = new QxBridge("C:/nope/does-not-exist/qx.exe");
  await within(5000, assert.rejects(bridge.call("run", { command: "pwd" })), "spawn failure");
  await new Promise((resolve) => setTimeout(resolve, 200));
  assert.equal(uncaught, null, `uncaught exception escaped the bridge: ${uncaught}`);
}

// 2. A child that dies mid-line must not poison the next response.
{
  const bridge = new QxBridge("unused");
  bridge.onData('{"jsonrpc":"2.0","id":9,"res');
  bridge.fail(new Error("died"));
  const next = new Promise((resolve, reject) => bridge.pending.set(1, { resolve, reject }));
  bridge.onData('{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n');
  assert.deepEqual(await within(1000, next, "response after a partial line"), { ok: true });
}

// 3. A late event from a dead child is ignored by the new one.
{
  const bridge = new QxBridge("unused");
  const stale = { id: "old" };
  const fresh = { id: "new" };
  bridge.child = fresh;
  const failures = [];
  bridge.fail = (error) => failures.push(error);
  bridge.failFrom(stale, new Error("late exit from an old child"));
  assert.equal(failures.length, 0, "a stale child's event must not fail the current one");
  bridge.failFrom(fresh, new Error("current child died"));
  assert.equal(failures.length, 1);
}

// A child that starts and never answers. The bridge runs `<exe> mcp`, so with node as the exe
// `mcp` is a script path in the current folder. It reads stdin and quits when the pipe closes,
// so a failed run never leaves it behind.
const silentDir = mkdtempSync(join(tmpdir(), "qx-pi-silent-"));
writeFileSync(
  join(silentDir, "mcp"),
  "process.stdin.resume();\nprocess.stdin.on('end', () => process.exit(0));\nprocess.stdin.on('error', () => process.exit(0));\nsetTimeout(() => process.exit(0), 60000);\n",
);
const silentBridge = () => {
  const bridge = new QxBridge(process.execPath);
  const home = process.cwd();
  process.chdir(silentDir);
  return {
    bridge,
    // spawn runs synchronously inside call(), so the folder can go back at once.
    call(signal) {
      try {
        return bridge.call("run", { program: "pwd" }, signal);
      } finally {
        process.chdir(home);
      }
    },
  };
};
const exited = (child) =>
  new Promise((resolve) => {
    if (child.exitCode !== null || child.signalCode !== null) resolve();
    else child.once("exit", resolve);
  });

try {
  // 4. A qx that never answers initialize fails the call after the start timeout and is killed.
  {
    const { bridge, call } = silentBridge();
    bridge.startTimeoutMs = 300;
    try {
      const pending = call();
      const child = bridge.child;
      await within(3000, assert.rejects(pending, /initialize/), "start timeout");
      await within(3000, exited(child), "silent child killed after the start timeout");
      assert.equal(bridge.ready, null, "the next call must start a fresh child");
    } finally {
      bridge.close();
    }
  }

  // 5. An abort while qx is still starting fails that call at once.
  {
    const { bridge, call } = silentBridge();
    const controller = new AbortController();
    try {
      const pending = call(controller.signal);
      setTimeout(() => controller.abort(), 200);
      await within(2000, assert.rejects(pending, /aborted/), "abort during start");
    } finally {
      bridge.close();
    }
  }
} finally {
  try {
    rmSync(silentDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  } catch {}
}

// 6. Finished calls leave no abort listener behind, so a later abort cancels nothing they did.
{
  const bridge = new QxBridge("unused");
  const sent = [];
  bridge.ready = Promise.resolve();
  bridge.child = {
    killed: false,
    stdin: {
      writable: true,
      write(line) {
        const message = JSON.parse(line);
        sent.push(message);
        if (message.id === undefined) return;
        // Every third call fails, so the error path is covered too.
        const reply =
          message.id % 3 === 0
            ? { jsonrpc: "2.0", id: message.id, error: { message: "no" } }
            : { jsonrpc: "2.0", id: message.id, result: { content: [] } };
        setImmediate(() => bridge.onData(`${JSON.stringify(reply)}\n`));
      },
    },
  };
  const controller = new AbortController();
  const calls = [];
  for (let i = 0; i < 200; i++) {
    calls.push(bridge.call("run", { program: "pwd" }, controller.signal));
  }
  const settled = await within(5000, Promise.allSettled(calls), "200 calls");
  assert.ok(settled.some((s) => s.status === "rejected"), "the error replies must reject");
  assert.equal(getEventListeners(controller.signal, "abort").length, 0, "abort listeners left behind");
  controller.abort();
  const cancelled = sent.filter((m) => m.method === "notifications/cancelled");
  assert.equal(cancelled.length, 0, "an abort after the replies must cancel nothing");
}

// 7. An abort while a call runs still cancels it in qx and rejects it.
{
  const bridge = new QxBridge("unused");
  const sent = [];
  bridge.ready = Promise.resolve();
  bridge.child = {
    killed: false,
    stdin: { writable: true, write: (line) => sent.push(JSON.parse(line)) },
  };
  const controller = new AbortController();
  const pending = bridge.call("run", { program: "pwd" }, controller.signal);
  await new Promise((resolve) => setImmediate(resolve));
  controller.abort();
  await within(1000, assert.rejects(pending, /aborted/), "abort of a running call");
  const request = sent.find((m) => m.method === "tools/call");
  const cancel = sent.find((m) => m.method === "notifications/cancelled");
  assert.equal(cancel?.params?.requestId, request.id);
  assert.equal(bridge.pending.size, 0);
}

console.log("bridge ok");
