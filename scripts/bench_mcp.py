"""Time qx MCP calls end to end over stdio. Read-only calls only.
usage: python bench_mcp.py <abs path to qx.exe> <repo cwd> [client]
"""
import json, statistics, subprocess, sys, time

exe, cwd = sys.argv[1], sys.argv[2]
client = sys.argv[3] if len(sys.argv) > 3 else "claude-code"
t_spawn = time.perf_counter()
p = subprocess.Popen([exe, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.DEVNULL)  # default buffering, never bufsize=0
nid = 0


def call(method, params=None, notify=False):
    global nid
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        msg["params"] = params
    if not notify:
        nid += 1
        msg["id"] = nid
    t = time.perf_counter()
    p.stdin.write((json.dumps(msg) + "\n").encode())
    p.stdin.flush()
    if notify:
        return None, 0.0
    line = p.stdout.readline()
    return json.loads(line), (time.perf_counter() - t) * 1000


_, init_ms = call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": client, "version": "2"}})
first_ms = (time.perf_counter() - t_spawn) * 1000
call("notifications/initialized", notify=True)

CASES = [
    ("run echo (builtin)", "run", {"program": "echo", "args": ["hi"], "cwd": cwd}),
    ("run ls (builtin)", "run", {"program": "ls", "args": ["src"], "cwd": cwd}),
    ("run git --version (spawn)", "run", {"program": "git", "args": ["--version"], "cwd": cwd}),
    ("run git status (spawn)", "run", {"program": "git", "args": ["status", "--short"], "cwd": cwd}),
    ("run cmd line (a && b)", "run", {"command": "echo a && echo b", "cwd": cwd}),
    ("run powershell line", "run", {"command": "Get-Date | Out-String", "cwd": cwd}),
    ("run bash pipeline", "run", {"command": "git log --oneline -5 | wc -l", "cwd": cwd}),
    ("run cargo --version", "run", {"program": "cargo", "args": ["--version"], "cwd": cwd}),
    ("run node -e 1", "run", {"program": "node", "args": ["-e", "1"], "cwd": cwd}),
    ("run python -c pass", "run", {"program": "python", "args": ["-c", "pass"], "cwd": cwd}),
    ("fs read Cargo.toml", "fs", {"op": "read", "path": "Cargo.toml", "cwd": cwd}),
    ("fs read src/fsops.rs (2000 lines)", "fs", {"op": "read", "path": "src/fsops.rs", "cwd": cwd}),
    ("fs stat", "fs", {"op": "stat", "path": "Cargo.toml", "cwd": cwd}),
    ("fs list src", "fs", {"op": "list", "path": "src", "cwd": cwd}),
    ("fs glob src/**/*.rs", "fs", {"op": "glob", "pattern": "src/**/*.rs", "cwd": cwd}),
    ("fs glob **/*.md", "fs", {"op": "glob", "pattern": "**/*.md", "cwd": cwd}),
    ("fs grep literal 'fn main'", "fs", {"op": "grep", "pattern": "fn main", "cwd": cwd}),
    ("fs grep regex Regex::new", "fs", {"op": "grep", "pattern": "Regex::new\\(", "cwd": cwd}),
    ("fs grep common 'let '", "fs", {"op": "grep", "pattern": "let ", "path": "src", "cwd": cwd}),
    ("fs batch 5 reads", "fs", {"cwd": cwd, "paths": ["Cargo.toml", "Map.md", "Goal.md", "CLAUDE.md", "Gatcha.md"]}),
]
N = 12
print(f"client={client} first reply (spawn+initialize) {first_ms:.1f} ms, initialize alone {init_ms:.2f} ms")
print(f"{'case':38} {'1st':>8} {'median':>8} {'p90':>8} {'bytes':>7} {'~tok':>6}")
for name, tool, args in CASES:
    ts, size = [], 0
    for i in range(N):
        r, ms = call("tools/call", {"name": tool, "arguments": args})
        ts.append(ms)
        if i == 0:
            res = r.get("result", {})
            size = sum(len(c.get("text", "")) for c in res.get("content", []))
            sc = res.get("structuredContent")
            if sc is not None:
                size += len(json.dumps(sc))
    rest = sorted(ts[1:])
    print(f"{name:38} {ts[0]:8.1f} {statistics.median(rest):8.1f} {rest[int(len(rest)*0.9)-1]:8.1f} {size:7d} {size//4:6d}")
p.stdin.close()
p.wait(timeout=10)
