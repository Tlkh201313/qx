// Exercises the qx bridge the way the pi extension does: initialize, then `run` and `fs`
// calls, and checks the extension's tool schemas against the ones qx serves.
// Usage: node integrations/pi/smoke.mjs <path-to-qx.exe>   (Node 22.15 or newer)
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { QxBridge } from "./qx/bridge.js";

const exe = process.argv[2];
if (!exe) {
  console.error("usage: node integrations/pi/smoke.mjs <path-to-qx.exe>");
  process.exit(2);
}

// qx.js imports `typebox`, which only pi supplies. This stand-in builds the same JSON Schema
// shape (properties, required, description), which is all the checks below read.
const TYPEBOX = `
const json = (schema, options) => ({ ...schema, ...(options ?? {}) });
export const Type = {
  Object: (properties, options) => json({
    type: "object",
    properties,
    required: Object.keys(properties).filter((key) => !properties[key].optional),
  }, options),
  Optional: (schema) => ({ ...schema, optional: true }),
  String: (options) => json({ type: "string" }, options),
  Number: (options) => json({ type: "number" }, options),
  Boolean: (options) => json({ type: "boolean" }, options),
  Array: (items, options) => json({ type: "array", items }, options),
  Literal: (value, options) => json({ const: value }, options),
  Union: (anyOf, options) => json({ anyOf }, options),
  Record: (key, value, options) => json({ type: "object", additionalProperties: value }, options),
};`;
registerHooks({
  resolve(specifier, context, next) {
    if (specifier !== "typebox") return next(specifier, context);
    return { url: `data:text/javascript,${encodeURIComponent(TYPEBOX)}`, shortCircuit: true };
  },
});
const extension = {};
const { default: register } = await import("./qx.js");
register({ registerTool: (tool) => (extension[tool.name.replace("mcp__qx__", "")] = tool) });

const bridge = new QxBridge(exe);
const dir = mkdtempSync(join(tmpdir(), "qx-pi-smoke-"));
try {
  const run = await bridge.call("run", { program: "pwd" });
  if (run.isError) throw new Error(`run failed: ${run.text}`);
  if (!run.text.trim()) throw new Error("run returned no output");

  const stat = await bridge.call("fs", {
    op: "stat",
    cwd: process.cwd(),
    path: ".",
  });
  if (stat.isError) throw new Error(`fs failed: ${stat.text}`);
  if (stat.structured?.code !== undefined && stat.structured.code !== "FS_OK" && stat.structured.path === undefined) {
    throw new Error(`fs stat returned an unexpected shape: ${JSON.stringify(stat.structured)}`);
  }

  // Every argument qx takes must be one pi lets the model send, and the extension must not
  // require one qx can do without (`cwd` is required on purpose: pi always has one).
  const { tools } = await bridge.request("tools/list", {});
  for (const name of ["run", "fs"]) {
    const served = tools.find((tool) => tool.name === name)?.inputSchema;
    assert.ok(served, `qx serves no ${name} tool`);
    const mine = extension[name]?.parameters;
    assert.ok(mine, `the extension registers no ${name} tool`);
    const missing = Object.keys(served.properties).filter((key) => !(key in mine.properties));
    assert.deepEqual(missing, [], `${name}: arguments qx takes that the extension does not offer`);
    const extra = mine.required.filter((key) => key !== "cwd" && !(served.required ?? []).includes(key));
    assert.deepEqual(extra, [], `${name}: arguments the extension requires and qx does not`);
  }

  // The words the model reads must match what qx does.
  const fs = extension.fs;
  assert.match(fs.description, /grep \(regex/, "fs text must say grep is a regex");
  assert.match(fs.parameters.properties.pattern.description, /regex/);
  assert.match(fs.parameters.properties.pattern.description, /literal/);
  assert.doesNotMatch(fs.description, /edit need/, "edit needs no prior read");
  const served = tools.find((tool) => tool.name === "run").description;
  const builtins = served.match(/Common tools \(([^)]+)\)/)?.[1]?.split(/\s+/);
  assert.ok(builtins?.length, "qx run text no longer lists its in-process tools; update this check");
  const unnamed = builtins.filter((name) => !new RegExp(`\\b${name}\\b`).test(extension.run.description));
  assert.deepEqual(unnamed, [], "in-process tools the run text leaves out");
  assert.match(extension.run.parameters.properties.timeout_ms.description, /270000/);

  // A pattern with regex characters is found as typed with `literal`.
  writeFileSync(join(dir, "code.js"), "arr[0] = 1;\ntotal += price * 2;\nfoo.bar();\nfooxbar();\n");
  const grep = await bridge.call("fs", { op: "grep", cwd: dir, pattern: "arr[0]", literal: true });
  assert.ok(!grep.isError, `literal grep failed: ${grep.text}`);
  assert.match(grep.text, /arr\[0\] = 1;/);
  console.log("pi bridge ok");
} finally {
  bridge.close();
  // The killed qx can still hold the folder for a moment; a leftover temp folder is no failure.
  try {
    rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  } catch {}
}
