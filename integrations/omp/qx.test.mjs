import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dir = mkdtempSync(join(tmpdir(), "qx-omp-"));

// The extension runs `<EXE> <subcommand> ...`: point EXE at node and let node run a script named
// after the subcommand in the cwd. The stubs below stand in for `qx cat`, `qx history` and `qx squeeze`.
process.chdir(dir);
const launcher = process.execPath;

const src = readFileSync(join(here, "qx.js"), "utf8").replaceAll("__QX_EXE__", launcher.replace(/\\/g, "\\\\"));
const ext = join(dir, "qx.mjs");
writeFileSync(ext, src);
const { default: register } = await import(pathToFileURL(ext).href);

// Fake omp: only `on` is used.
function load() {
  const handlers = {};
  register({ on: (name, fn) => (handlers[name] ??= []).push(fn) });
  return { handlers };
}

test("registers no tool and only the expected handlers", () => {
  const registered = [];
  const handlers = {};
  register({
    registerTool: (t) => registered.push(t),
    getAllTools: () => assert.fail("getAllTools must not be called"),
    on: (name, fn) => (handlers[name] ??= []).push(fn),
  });
  assert.deepEqual(registered, []);
  assert.deepEqual(Object.keys(handlers).sort(), ["session.compacting", "session_shutdown", "session_start", "tool_result"]);
});

// Stubs for `qx cat <file>` and `qx history --brief`: behaviour comes from the `mode` file in the
// stub dir ("ok", "empty", "fail"), so each test picks one without reloading the extension. Each run
// appends its name to `ran`, so a test can tell whether the stub was spawned.
const stub = (name, out) =>
  writeFileSync(
    join(dir, name),
    `const fs = require("node:fs");
fs.appendFileSync(${JSON.stringify(join(dir, "ran"))}, ${JSON.stringify(name + "\n")});
const m = fs.readFileSync(${JSON.stringify(join(dir, "mode"))}, "utf8");
if (m === "fail") process.exit(1);
if (m === "ok") process.stdout.write(${JSON.stringify(out)});
`,
  );
stub("cat", "digest line 1\ndigest line 2\n");
stub("history", "git status\ncargo test\n");
const setMode = (m) => writeFileSync(join(dir, "mode"), m);
const ran = () => (existsSync(join(dir, "ran")) ? readFileSync(join(dir, "ran"), "utf8") : "");
const clearRan = () => rmSync(join(dir, "ran"), { force: true });

const BIG = "x".repeat(7000);
const readEvent = (path, over = {}) => ({
  toolName: "read",
  toolCallId: "t1",
  input: { path },
  content: [{ type: "text", text: BIG }],
  isError: false,
  ...over,
});
const fire = async (name, ...args) => {
  const { handlers } = load();
  assert.equal(handlers[name]?.length, 1);
  return handlers[name][0](...args);
};
const ctx = { cwd: dir };

test("tool_result replaces a big .json read with the digest and a line-range footer", async () => {
  setMode("ok");
  writeFileSync(join(dir, "data.json"), "a\nb\nc\n");
  const r = await fire("tool_result", readEvent("data.json"), ctx);
  assert.equal(r.content.length, 1);
  assert.equal(r.content[0].type, "text");
  assert.equal(
    r.content[0].text,
    "digest line 1\ndigest line 2\n[qx] digest of data.json; read data.json with a line range (data.json:N-M) for exact text",
  );
});

test("tool_result digests a read of an absolute path", async () => {
  setMode("ok");
  const abs = join(dir, "data.json");
  writeFileSync(abs, "a\n");
  const r = await fire("tool_result", readEvent(abs), ctx);
  assert.ok(r.content[0].text.endsWith(`[qx] digest of ${abs}; read ${abs} with a line range (${abs}:N-M) for exact text`));
});

test("tool_result leaves a missing file, URL and internal URI alone without spawning", async () => {
  setMode("ok");
  writeFileSync(join(dir, "data.json"), "a\n");
  clearRan();
  for (const p of ["missing.json", "https://example.com/data.json", "artifact://3.json", "subdir.json"]) {
    assert.equal(await fire("tool_result", readEvent(p), ctx), undefined, p);
  }
  assert.equal(ran(), "", "qx cat must not run");
});

test("tool_result leaves ranged, other-extension, small, error and failing-exe reads alone", async () => {
  setMode("ok");
  writeFileSync(join(dir, "data.json"), "a\n");
  for (const ev of [
    readEvent("data.json:1-50"),
    readEvent("data.json:raw"),
    readEvent("data.json:+20"),
    readEvent("main.rs"),
    readEvent("data.json", { content: [{ type: "text", text: "x".repeat(6000) }] }),
    readEvent("data.json", { isError: true }),
    readEvent("data.json", { toolName: "write" }),
  ]) {
    assert.equal(await fire("tool_result", ev, ctx), undefined, JSON.stringify(ev.input));
  }
  setMode("fail");
  assert.equal(await fire("tool_result", readEvent("data.json"), ctx), undefined);
  setMode("empty");
  assert.equal(await fire("tool_result", readEvent("data.json"), ctx), undefined);
});

test("tool_result keeps the original when the digest is not under 70% of it", async () => {
  setMode("ok");
  writeFileSync(join(dir, "data.json"), "a\n");
  const ev = readEvent("data.json", { content: [{ type: "text", text: "y".repeat(6500) }] });
  const { handlers } = load();
  // A 5000-char digest is over 70% of the 6500-char original.
  stub("cat", "z".repeat(5000));
  assert.equal(await handlers.tool_result[0](ev, ctx), undefined);
  stub("cat", "digest line 1\ndigest line 2\n");
});

// Stub `qx squeeze ...`: it records its args and stdin, and in mode "ok" prints a short digest.
writeFileSync(
  join(dir, "squeeze"),
  `const fs = require("node:fs");
const m = fs.readFileSync(${JSON.stringify(join(dir, "mode"))}, "utf8");
const input = fs.readFileSync(0, "utf8");
fs.writeFileSync(${JSON.stringify(join(dir, "squeezed.json"))}, JSON.stringify({ args: process.argv.slice(2), input }));
if (m === "fail") process.exit(1);
if (m === "ok") process.stdout.write("squeezed\\n[qx] full output: spool.log\\n");
if (m === "long") process.stdout.write("z".repeat(8000));
`,
);
const OUT = "src/a.rs:1:text\n".repeat(400);
const TRAILER = "\n\nWall time: 2.00 seconds\n\nCommand exited with code 1";
const bashEvent = (command, over = {}) => ({
  toolName: "bash",
  toolCallId: "t2",
  input: { command },
  content: [{ type: "text", text: OUT.trimEnd() + TRAILER }],
  details: { exitCode: 1 },
  isError: true,
  ...over,
});
const seen = () => JSON.parse(readFileSync(join(dir, "squeezed.json"), "utf8"));

test("tool_result replaces a big bash result with the digest and keeps the trailer", async () => {
  setMode("ok");
  const r = await fire("tool_result", bashEvent("cargo test"), ctx);
  assert.deepEqual(r, {
    content: [{ type: "text", text: "squeezed\n[qx] full output: spool.log" + TRAILER }],
  });
  assert.ok(r.content[0].text.endsWith("Wall time: 2.00 seconds\n\nCommand exited with code 1"));
});

test("tool_result runs qx squeeze with the model's command, exit code, cwd and ratio", async () => {
  setMode("ok");
  await fire("tool_result", bashEvent("cargo test -- --nocapture | head"), ctx);
  const s = seen();
  assert.deepEqual(s.args, [
    "--label",
    "cargo test -- --nocapture | head",
    "--code",
    "1",
    "--cwd",
    dir,
    "--max-ratio",
    "70",
  ]);
  // stdin is the body alone: no trailer.
  assert.equal(s.input, OUT.trimEnd());
});

test("tool_result resolves input.cwd against ctx.cwd", async () => {
  setMode("ok");
  await fire("tool_result", bashEvent("ls", { input: { command: "ls", cwd: "sub" } }), ctx);
  const args = seen().args;
  assert.equal(args[args.indexOf("--cwd") + 1], resolve(dir, "sub"));
});

test("tool_result takes the exit code from the notice, then the timeout flag, else 0", async () => {
  setMode("ok");
  const text = (tail) => [{ type: "text", text: OUT.trimEnd() + tail }];
  const code = () => seen().args[seen().args.indexOf("--code") + 1];
  await fire("tool_result", bashEvent("a", { details: undefined, content: text("\n\nCommand exited with code 7") }), ctx);
  assert.equal(code(), "7");
  await fire("tool_result", bashEvent("a", { details: { timedOut: true }, content: text("\n\n[Command timed out after 5 seconds]") }), ctx);
  assert.equal(code(), "124");
  await fire("tool_result", bashEvent("a", { details: {}, isError: false, content: text("\n\nWall time: 1.20 seconds") }), ctx);
  assert.equal(code(), "0");
  await fire("tool_result", bashEvent("a", { details: undefined, isError: false, content: text("") }), ctx);
  assert.equal(code(), "0");
});

test("tool_result keeps every omp notice verbatim after the digest", async () => {
  setMode("ok");
  const tail =
    "\n\nWall time: 12.00 seconds\n[raw output: artifact://4]\n[Output truncated: showing 400 of 900 lines]" +
    "\nRan in a copy of the busy session shell; changes here won't persist.\n\nCommand exited with code 2";
  const r = await fire("tool_result", bashEvent("make", { content: [{ type: "text", text: OUT.trimEnd() + tail }] }), ctx);
  assert.equal(r.content[0].text, "squeezed\n[qx] full output: spool.log" + tail);
  assert.equal(seen().input, OUT.trimEnd());
});

test("tool_result digests a successful bash result", async () => {
  setMode("ok");
  const ev = bashEvent("git log", { details: {}, isError: false, content: [{ type: "text", text: OUT }] });
  const r = await fire("tool_result", ev, ctx);
  assert.equal(r.content[0].text.startsWith("squeezed"), true);
});

test("tool_result leaves failing, empty, not-shorter and non-text bash results alone", async () => {
  for (const m of ["fail", "empty", "long"]) {
    setMode(m);
    assert.equal(await fire("tool_result", bashEvent("cargo test"), ctx), undefined, m);
  }
  setMode("ok");
  rmSync(join(dir, "squeezed.json"), { force: true });
  for (const ev of [
    bashEvent("x", { content: [{ type: "text", text: OUT }, { type: "image", data: "" }] }),
    bashEvent("x", { content: [{ type: "image", data: "" }] }),
    bashEvent("x", { content: undefined }),
    bashEvent("x", { content: [{ type: "text", text: "" + TRAILER }] }),
    bashEvent("x", { content: [{ type: "text", text: "  \n" }] }),
    bashEvent("x", { input: {} }),
    bashEvent("x", { input: { command: "" } }),
    bashEvent("x", { toolName: "grep" }),
  ]) {
    assert.equal(await fire("tool_result", ev, ctx), undefined, JSON.stringify(ev.content));
  }
  assert.ok(!existsSync(join(dir, "squeezed.json")), "squeeze must not even run");
});

test("session.compacting returns qx history lines as context", async () => {
  setMode("ok");
  const r = await fire("session.compacting", { type: "session.compacting", sessionId: "s", messages: [] }, ctx);
  assert.deepEqual(r, {
    context: [
      "Commands already run in this repo (from qx history; re-run only if files changed):",
      "git status",
      "cargo test",
    ],
  });
});

test("session.compacting returns nothing when qx prints nothing or fails", async () => {
  for (const m of ["empty", "fail"]) {
    setMode(m);
    assert.equal(await fire("session.compacting", { type: "session.compacting" }, ctx), undefined);
  }
});
