// Exercises the qx bridge the way the pi extension does: initialize, then one
// `run` and one `fs` call. Usage: node integrations/pi/smoke.mjs <path-to-qx.exe>
import { QxBridge } from "./qx/bridge.js";

const exe = process.argv[2];
if (!exe) {
  console.error("usage: node integrations/pi/smoke.mjs <path-to-qx.exe>");
  process.exit(2);
}

const bridge = new QxBridge(exe);
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
  console.log("pi bridge ok");
} finally {
  bridge.close();
}
