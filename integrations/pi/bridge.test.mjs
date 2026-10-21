// Run: node integrations/pi/bridge.test.mjs   (no qx binary needed)
import assert from "node:assert/strict";
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

console.log("bridge ok");
