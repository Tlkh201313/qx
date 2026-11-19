#!/usr/bin/env python3
"""Prove every client that `qx setup` registers really starts and serves qx.

For each client it reads the config exactly as the client would, spawns the configured
command with the configured args and env, runs an MCP session (initialize, tools/list,
one `run`, one `fs` read), then checks the client's hook where setup writes one.
It never writes to a config. Exit 1 on any failure.

    python scripts/check_clients.py [--only omp,codex] [--timeout 30] [--repo PATH]
"""
from __future__ import annotations

import argparse
import json
import os
import queue
import re
import shutil
import subprocess
import sys
import threading
import time
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

PROTOCOL = "2025-06-18"
HOME = Path(os.environ.get("USERPROFILE") or os.environ.get("HOME") or "~").expanduser()
LOCAL = Path(os.environ.get("LOCALAPPDATA") or HOME / "AppData" / "Local")
REPO = Path(__file__).resolve().parent.parent


def env_path(key: str, default: Path) -> Path:
    v = os.environ.get(key)
    return Path(v) if v else default


DSH_HOME = env_path("DSH_HOME", HOME / ".dsh")
HERMES_HOME = env_path("HERMES_HOME", LOCAL / "hermes")
CODEX_HOME = env_path("CODEX_HOME", HOME / ".codex")
CLAUDE_DIR = os.environ.get("CLAUDE_CONFIG_DIR")
OMP_DIR = env_path("PI_CODING_AGENT_DIR", HOME / (os.environ.get("PI_CONFIG_DIR") or ".omp") / "agent")

# What each client's server instructions must name as the run tool (client.rs `tool_id`).
RUN_ID = {
    "dsh": r"mcp__qx__run",
    "claude-code": r"mcp__qx__run",
    "codex": r"mcp__qx__run",
    "hermes": r"mcp__qx__run",
    "pi": r"mcp__qx__run",
    "opencode": r"(?<![\w])qx_run",
    "omp": r"mcp__qx_run",
    "cursor": r"user-qx",
}


class Fail(Exception):
    pass


@dataclass
class Spawn:
    command: str
    args: list[str]
    env: dict[str, str]
    config: str  # file[:line] shown in the table and in failures
    expect_client: str


@dataclass
class Result:
    client: str
    config: str = "-"
    spawn: str = "-"
    tools: str = "-"
    run: str = "-"
    fs: str = "-"
    hook: str = "-"
    ms: int = 0
    failures: list[str] = field(default_factory=list)

    def fail(self, col: str, why: str) -> None:
        setattr(self, col, "FAIL")
        self.failures.append(f"{col}: {why}")


# ---------------------------------------------------------------- config parsing


def strip_jsonc(text: str) -> str:
    """Comments and trailing commas out, strings untouched."""
    out: list[str] = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out.append(text[i : j + 1])
            i = j + 1
        elif text.startswith("//", i):
            while i < n and text[i] != "\n":
                i += 1
        elif text.startswith("/*", i):
            i = text.find("*/", i + 2)
            i = n if i < 0 else i + 2
        else:
            out.append(c)
            i += 1
    return re.sub(r",(\s*[}\]])", r"\1", "".join(out))


def load_json(path: Path) -> dict:
    try:
        text = path.read_text(encoding="utf-8-sig")
    except FileNotFoundError:
        raise Fail(f"{path}: file missing")
    try:
        return json.loads(strip_jsonc(text))
    except json.JSONDecodeError as e:
        raise Fail(f"{path}:{e.lineno}: not valid JSON ({e.msg})")


def line_of(path: Path, needle: str) -> str:
    """`path:LINE` of the first line holding the qx entry, else just `path`."""
    try:
        for n, line in enumerate(path.read_text(encoding="utf-8-sig").splitlines(), 1):
            if needle in line:
                return f"{path}:{n}"
    except OSError:
        pass
    return str(path)


def dig(obj, *keys):
    for k in keys:
        if not isinstance(obj, dict) or k not in obj:
            return None
        obj = obj[k]
    return obj


def mcp_json(path: Path, client: str, *keys: str) -> Spawn:
    entry = dig(load_json(path), *keys)
    where = line_of(path, '"qx"')
    if not isinstance(entry, dict):
        raise Fail(f"{path}: no {'.'.join(keys)} entry")
    if entry.get("enabled") is False or entry.get("disabled") is True:
        raise Fail(f"{where}: qx entry is disabled")
    cmd = entry.get("command")
    args = entry.get("args", [])
    if isinstance(cmd, list):  # opencode: command is the whole argv
        cmd, args = cmd[0], cmd[1:]
    if not isinstance(cmd, str) or not cmd:
        raise Fail(f"{where}: qx entry has no command")
    env = entry.get("env") or entry.get("environment") or {}
    return Spawn(cmd, [str(a) for a in args], {k: str(v) for k, v in env.items()}, where, client)


def codex() -> Spawn:
    path = CODEX_HOME / "config.toml"
    try:
        with path.open("rb") as f:
            data = tomllib.load(f)
    except FileNotFoundError:
        raise Fail(f"{path}: file missing")
    except tomllib.TOMLDecodeError as e:
        raise Fail(f"{path}: {e}")
    entry = dig(data, "mcp_servers", "qx")
    where = line_of(path, "[mcp_servers.qx]")
    if not isinstance(entry, dict):
        raise Fail(f"{path}: no [mcp_servers.qx] table")
    if entry.get("enabled") is False:
        raise Fail(f"{where}: qx entry is disabled")
    return Spawn(
        entry["command"], [str(a) for a in entry.get("args", [])], {k: str(v) for k, v in (entry.get("env") or {}).items()}, where, "codex"
    )


def unquote(v: str) -> str:
    v = v.strip()
    if len(v) >= 2 and v[0] == v[-1] and v[0] in "'\"":
        inner = v[1:-1]
        return inner.replace("''", "'") if v[0] == "'" else json.loads(v)
    return v


def indent_of(line: str) -> int:
    return len(line) - len(line.lstrip(" "))


def yaml_block(lines: list[str], start: int, base: int) -> dict:
    """Scalars, flow lists, block lists and one level of nested maps under lines[start-1].

    Minimal on purpose: handles exactly what `qx setup` writes for hermes and dsh.
    """
    out: dict = {}
    i = start
    while i < len(lines):
        line = lines[i]
        if not line.strip() or line.lstrip().startswith("#"):
            i += 1
            continue
        ind = indent_of(line)
        if ind <= base:
            break
        key, _, val = line.strip().partition(":")
        val = val.strip()
        if val.startswith("[") and val.endswith("]"):
            out[key] = [unquote(x) for x in val[1:-1].split(",") if x.strip()]
        elif val:
            out[key] = unquote(val)
        else:
            j = i + 1
            while j < len(lines) and (not lines[j].strip() or indent_of(lines[j]) > ind):
                j += 1
            body = [x for x in lines[i + 1 : j] if x.strip() and not x.lstrip().startswith("#")]
            if body and body[0].lstrip().startswith("- "):
                out[key] = [unquote(x.lstrip()[2:]) for x in body]
            else:
                out[key] = yaml_block(lines, i + 1, ind)
            i = j
            continue
        i += 1
    return out


def hermes() -> Spawn:
    path = HERMES_HOME / "config.yaml"
    try:
        lines = path.read_text(encoding="utf-8-sig").splitlines()
    except FileNotFoundError:
        raise Fail(f"{path}: file missing")
    top = next((i for i, l in enumerate(lines) if l.rstrip() == "mcp_servers:"), None)
    if top is None:
        raise Fail(f"{path}: no top-level mcp_servers:")
    qx = next((i for i in range(top + 1, len(lines)) if lines[i].strip() == "qx:" and indent_of(lines[i]) > 0), None)
    if qx is None:
        raise Fail(f"{path}: no qx entry under mcp_servers:")
    e = yaml_block(lines, qx + 1, indent_of(lines[qx]))
    where = f"{path}:{qx + 1}"
    if str(e.get("enabled", "true")).lower() == "false":
        raise Fail(f"{where}: qx entry is disabled")
    if "command" not in e:
        raise Fail(f"{where}: qx entry has no command")
    return Spawn(e["command"], list(e.get("args", [])), dict(e.get("env", {})), where, "hermes")


def dsh() -> Spawn:
    path = DSH_HOME / "cordis.patch.yml"
    try:
        lines = path.read_text(encoding="utf-8-sig").splitlines()
    except FileNotFoundError:
        raise Fail(f"{path}: file missing")
    mark = next((i for i, l in enumerate(lines) if re.match(r"\s*-?\s*id:\s*mcp-qx\s*$", l)), None)
    if mark is None:
        raise Fail(f"{path}: no `id: mcp-qx` entry")
    cfg = next((i for i in range(mark, len(lines)) if lines[i].strip() == "config:"), None)
    if cfg is None:
        raise Fail(f"{path}:{mark + 1}: mcp-qx entry has no config:")
    e = yaml_block(lines, cfg + 1, indent_of(lines[cfg]))
    where = f"{path}:{cfg + 1}"
    if e.get("transport") != "stdio" or "command" not in e:
        raise Fail(f"{where}: not a stdio entry with a command")
    return Spawn(e["command"], list(e.get("args", [])), dict(e.get("env", {})), where, "dsh")


def pi_extension() -> Spawn:
    """pi older than 0.99 has no MCP config: its extension spawns `EXE mcp` with QX_CLIENT=pi."""
    mcp = HOME / ".pi" / "agent" / "mcp.json"
    if mcp.exists():
        return mcp_json(mcp, "pi", "mcpServers", "qx")
    ext = HOME / ".pi" / "agent" / "extensions" / "qx.js"
    try:
        text = ext.read_text(encoding="utf-8")
    except FileNotFoundError:
        raise Fail(f"{ext}: file missing (and no {mcp})")
    m = re.search(r'^const EXE = (".*");', text, re.M)
    if not m:
        raise Fail(f"{ext}: no `const EXE = \"...\";` line")
    exe = json.loads(m.group(1))
    where = f"{ext}:{text[: m.start()].count(chr(10)) + 1}"
    bridge = re.search(r'spawn\(this\.exe, (\[[^\]]*\])', (ext.parent / "qx" / "bridge.js").read_text(encoding="utf-8")) if (ext.parent / "qx" / "bridge.js").exists() else None
    args = json.loads(bridge.group(1)) if bridge else ["mcp"]
    return Spawn(exe, args, {"QX_CLIENT": "pi"}, where, "pi")


CLIENTS = {
    "dsh": dsh,
    "claude-code": lambda: mcp_json(Path(CLAUDE_DIR or HOME) / ".claude.json", "claude-code", "mcpServers", "qx"),
    "codex": codex,
    "opencode": None,  # resolved below: which file holds qx
    "hermes": hermes,
    "pi": pi_extension,
    "cursor": lambda: mcp_json(HOME / ".cursor" / "mcp.json", "cursor", "mcpServers", "qx"),
    "omp": lambda: mcp_json(OMP_DIR / "mcp.json", "omp", "mcpServers", "qx"),
}


def opencode() -> Spawn:
    d = HOME / ".config" / "opencode"
    files = [d / "opencode.json", d / "opencode.jsonc"]
    for f in files:  # setup's own pick: the file that already holds qx
        try:
            data = json.loads(strip_jsonc(f.read_text(encoding="utf-8-sig")))
        except (OSError, json.JSONDecodeError):
            continue
        for keys in (("mcp", "servers", "qx"), ("mcp", "qx")):
            if isinstance(dig(data, *keys), dict):
                return mcp_json(f, "opencode", *keys)
    raise Fail(f"{d}: no mcp.servers.qx / mcp.qx entry in opencode.json or opencode.jsonc")


CLIENTS["opencode"] = opencode


# ---------------------------------------------------------------- MCP session


class Session:
    def __init__(self, sp: Spawn, timeout: float):
        env = {k: v for k, v in os.environ.items() if k not in ("QX_CLIENT", "CLAUDECODE")}
        env.update(sp.env)
        self.timeout = timeout
        self.proc = subprocess.Popen(
            [sp.command, *sp.args],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=str(HOME),
        )
        self.out: queue.Queue = queue.Queue()
        self.err: list[str] = []
        threading.Thread(target=self._pump, daemon=True).start()
        threading.Thread(target=lambda: [self.err.append(l.decode("utf-8", "replace")) for l in self.proc.stderr], daemon=True).start()
        self.next_id = 0

    def _pump(self) -> None:
        for raw in self.proc.stdout:
            try:
                self.out.put(json.loads(raw))
            except json.JSONDecodeError:
                self.out.put({"_garbage": raw.decode("utf-8", "replace")})
        self.out.put(None)

    def send(self, msg: dict) -> None:
        self.proc.stdin.write((json.dumps(msg) + "\n").encode())
        self.proc.stdin.flush()

    def request(self, method: str, params: dict | None = None) -> dict:
        self.next_id += 1
        rid = self.next_id
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}})
        deadline = time.perf_counter() + self.timeout
        while True:
            try:
                msg = self.out.get(timeout=max(0.01, deadline - time.perf_counter()))
            except queue.Empty:
                raise Fail(f"{method}: no reply in {self.timeout:.0f}s; stderr: {self.stderr()}")
            if msg is None:
                raise Fail(f"{method}: server exited (code {self.proc.poll()}); stderr: {self.stderr()}")
            if "_garbage" in msg:
                raise Fail(f"{method}: non-JSON line on stdout: {msg['_garbage'][:120]!r}")
            if msg.get("id") == rid:
                if "error" in msg:
                    raise Fail(f"{method}: {msg['error']}")
                return msg["result"]

    def notify(self, method: str) -> None:
        self.send({"jsonrpc": "2.0", "method": method})

    def stderr(self) -> str:
        return "".join(self.err)[-200:].strip() or "(none)"

    def close(self) -> None:
        try:
            self.proc.stdin.close()
            self.proc.wait(3)
        except Exception:
            self.proc.kill()


def text_of(result: dict) -> str:
    return "".join(c.get("text", "") for c in result.get("content", []) if c.get("type") == "text")


def check_mcp(sp: Spawn, r: Result, repo: str, timeout: float) -> int:
    t_all = time.perf_counter()
    exe = Path(sp.command)
    if not exe.exists() and not shutil.which(sp.command):
        r.fail("spawn", f"{sp.config}: command {sp.command!r} does not exist")
        return 0
    steps: dict[str, float] = {}
    s = None
    try:
        t = time.perf_counter()
        s = Session(sp, timeout)
        init = s.request(
            "initialize",
            {"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "qx-check", "version": "1"}},
        )
        s.notify("notifications/initialized")
        steps["spawn"] = time.perf_counter() - t
        name = dig(init, "serverInfo", "name")
        if name != "qx":
            raise Fail(f"initialize: serverInfo.name is {name!r}, not 'qx'")
        instr = init.get("instructions") or ""
        want = RUN_ID[sp.expect_client]
        if not re.search(want, instr):
            r.fail("spawn", f"{sp.config}: instructions do not name this client's run tool ({want}); QX_CLIENT in config={sp.env.get('QX_CLIENT')!r}; got: {instr[:80]!r}")
            s.close()
            return 0
        r.spawn = f"ok {steps['spawn'] * 1000:.0f}"
    except Fail as e:
        r.fail("spawn", f"{sp.config}: {e}")
        if s:
            s.close()
        return int((time.perf_counter() - t_all) * 1000)
    except OSError as e:
        r.fail("spawn", f"{sp.config}: cannot start {sp.command}: {e}")
        return int((time.perf_counter() - t_all) * 1000)

    try:
        t = time.perf_counter()
        tools = s.request("tools/list")["tools"]
        names = {x["name"] for x in tools}
        missing = {"run", "fs"} - names
        if missing:
            r.fail("tools", f"{sp.config}: tools/list lacks {sorted(missing)}; has {sorted(names)}")
        else:
            r.tools = f"ok {(time.perf_counter() - t) * 1000:.0f}"
    except Fail as e:
        r.fail("tools", f"{sp.config}: {e}")

    try:
        t = time.perf_counter()
        res = s.request("tools/call", {"name": "run", "arguments": {"program": "git", "args": ["--version"], "cwd": repo}})
        txt = text_of(res)
        if res.get("isError") or "git version" not in txt:
            r.fail("run", f"{sp.config}: run git --version -> isError={res.get('isError')} text={txt[:120]!r}")
        else:
            r.run = f"ok {(time.perf_counter() - t) * 1000:.0f}"
    except Fail as e:
        r.fail("run", f"{sp.config}: {e}")

    try:
        t = time.perf_counter()
        res = s.request(
            "tools/call",
            {"name": "fs", "arguments": {"op": "read", "path": f"{repo}/Cargo.toml", "offset": 1, "limit": 3}},
        )
        txt = text_of(res)
        if res.get("isError") or "[package]" not in txt:
            r.fail("fs", f"{sp.config}: fs read Cargo.toml 1-3 -> isError={res.get('isError')} text={txt[:120]!r}")
        else:
            r.fs = f"ok {(time.perf_counter() - t) * 1000:.0f}"
    except Fail as e:
        r.fail("fs", f"{sp.config}: {e}")
    s.close()
    return int((time.perf_counter() - t_all) * 1000)


# ---------------------------------------------------------------- hooks


def run_hook(argv: list[str], stdin: dict, cwd: str) -> dict | None:
    p = subprocess.run(argv, input=json.dumps(stdin), capture_output=True, text=True, timeout=15, cwd=cwd)
    if p.returncode != 0:
        raise Fail(f"{' '.join(argv)} exited {p.returncode}: {p.stderr.strip()[:150]}")
    out = p.stdout.strip()
    if not out:
        raise Fail(f"{' '.join(argv)} printed nothing: the hook did not rewrite `git status` (rule, PATH or rtk decision)")
    try:
        return json.loads(out)
    except json.JSONDecodeError:
        raise Fail(f"hook output is not JSON: {out[:120]!r}")


def is_qx_line(line: str) -> bool:
    return bool(re.match(r"^\s*(qx|\"?[^\s\"]*[\\/]qx(\.exe)?\"?)(\s|$)", line, re.I))


def hook_omp(repo: str) -> str:
    ext = OMP_DIR / "extensions" / "qx.js"
    try:
        text = ext.read_text(encoding="utf-8")
    except FileNotFoundError:
        raise Fail(f"{ext}: extension missing")
    if "__QX_EXE__" in text:
        raise Fail(f"{ext}: still holds the __QX_EXE__ placeholder")
    m = re.search(r'^const EXE = (".*");', text, re.M)
    if not m:
        raise Fail(f"{ext}: no `const EXE = \"...\";` line")
    exe = json.loads(m.group(1))
    line = text[: m.start()].count("\n") + 1
    if not Path(exe).exists():
        raise Fail(f"{ext}:{line}: EXE {exe} does not exist")
    if "invokeTool" in text:
        raise Fail(f"{ext}: still delegates through invokeTool (the old bash override); run `qx setup omp`")
    if "squeeze" not in text:
        raise Fail(f"{ext}: does not call `qx squeeze`; run `qx setup omp`")
    t = time.perf_counter()
    log = subprocess.run(["git", "log", "-n", "30"], capture_output=True, text=True, timeout=15, cwd=repo)
    if log.returncode != 0 or not log.stdout.strip():
        raise Fail(f"git log -n 30 in {repo} failed: {log.stderr.strip()[:150]}")
    p = subprocess.run(
        [exe, "squeeze", "--label", "git log -n 30", "--max-ratio", "70"],
        input=log.stdout, capture_output=True, text=True, timeout=15,
        env={**os.environ, "QX_HISTORY": "off"},
    )
    if p.returncode != 0:
        raise Fail(f"qx squeeze exited {p.returncode}: {p.stderr.strip()[:150]}")
    if not p.stdout.strip() or len(p.stdout) >= len(log.stdout):
        raise Fail(f"qx squeeze did not shorten `git log -n 30` ({len(log.stdout)} -> {len(p.stdout)} chars)")
    return f"ok {(time.perf_counter() - t) * 1000:.0f}"


def hook_pi() -> str:
    ext = HOME / ".pi" / "agent" / "extensions"
    for f in (ext / "qx.js", ext / "qx" / "bridge.js", ext / "qx" / "package.json"):
        if not f.exists():
            raise Fail(f"{f}: missing")
    text = (ext / "qx.js").read_text(encoding="utf-8")
    if "__QX_EXE__" in text:
        raise Fail(f"{ext / 'qx.js'}: still holds the __QX_EXE__ placeholder")
    m = re.search(r'^const EXE = (".*");', text, re.M)
    if not m:
        raise Fail(f"{ext / 'qx.js'}: no EXE line")
    exe = json.loads(m.group(1))
    if not Path(exe).exists():
        raise Fail(f"{ext / 'qx.js'}:{text[: m.start()].count(chr(10)) + 1}: EXE {exe} does not exist")
    return "ok ext"


def hook_claude(repo: str) -> str:
    base = Path(CLAUDE_DIR) if CLAUDE_DIR else HOME / ".claude"
    path = base / "settings.json"
    groups = dig(load_json(path), "hooks", "PreToolUse")
    if not isinstance(groups, list):
        raise Fail(f"{path}: no hooks.PreToolUse list")
    found = None
    for g in groups:
        for h in g.get("hooks", []):
            if h.get("args") == ["hook", "claude"] and Path(str(h.get("command", ""))).stem.lower() == "qx":
                found = (g, h)
    if not found:
        raise Fail(f"{path}: no PreToolUse hook running `qx hook claude`")
    g, h = found
    where = line_of(path, '"hook"')
    if not Path(h["command"]).exists():
        raise Fail(f"{where}: hook command {h['command']} does not exist")
    if not re.search(g.get("matcher", ""), "Bash"):
        raise Fail(f"{where}: matcher {g.get('matcher')!r} does not match Bash")
    t = time.perf_counter()
    argv = [h["command"], *h["args"]]
    notes = []
    # rtk's own PreToolUse hook (same matcher) owns `git status` & co. when installed, and qx then
    # leaves the line alone on purpose; a program rtk does not rewrite proves qx's hook instead.
    # In default permission mode the hook also stays silent unless the user's allow rules cover both the
    # original and the `qx ...` line (hook.rs `allows_both`); bypassPermissions shows the rewrite itself works.
    for mode, probe in (("default", "git status"), ("default", "cargo --version"), ("bypassPermissions", "cargo --version")):
        event = {"tool_name": "Bash", "tool_input": {"command": probe}, "cwd": repo, "permission_mode": mode}
        try:
            out = run_hook(argv, event, repo)
        except Fail as e:
            if "printed nothing" not in str(e):
                raise
            notes.append(f"`{probe}` left alone in {mode} mode")
            continue
        cmd = dig(out, "hookSpecificOutput", "updatedInput", "command") or ""
        if not is_qx_line(cmd):
            raise Fail(f"qx hook claude rewrote {probe!r} to {cmd!r}, not a `qx ...` line")
        NOTES.extend(f"claude-code hook: {n} ({path}: rtk's hook owns rtk commands; permissions.allow lacks the rewritten line)" for n in notes)
        return f"ok {(time.perf_counter() - t) * 1000:.0f}" + (" (bypass)" if mode != "default" else "")
    raise Fail(f"{path}: qx hook claude rewrote neither probe ({'; '.join(notes)})")


NOTES: list[str] = []


# ---------------------------------------------------------------- main


def check(name: str, repo: str, timeout: float) -> Result:
    r = Result(name)
    try:
        sp = CLIENTS[name]()
        r.config = sp.config
    except Fail as e:
        r.fail("config", str(e))
        r.spawn = r.tools = r.run = r.fs = "skip"
        sp = None
    except Exception as e:  # a parser bug must not hide the other clients
        r.fail("config", f"{type(e).__name__}: {e}")
        r.spawn = r.tools = r.run = r.fs = "skip"
        sp = None
    if sp:
        r.config = "ok"
        r.ms = check_mcp(sp, r, repo, timeout)
        r.config = f"ok {sp.config}"
    hook = {"omp": lambda: hook_omp(repo), "pi": hook_pi, "claude-code": lambda: hook_claude(repo)}.get(name)
    if hook:
        try:
            r.hook = hook()
        except Fail as e:
            r.fail("hook", str(e))
        except Exception as e:
            r.fail("hook", f"{type(e).__name__}: {e}")
    return r


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--only", help="comma-separated client keys")
    ap.add_argument("--timeout", type=float, default=30, help="seconds per MCP request")
    ap.add_argument("--repo", default=str(REPO).replace("\\", "/"), help="cwd for the run/fs probes")
    a = ap.parse_args()
    names = a.only.split(",") if a.only else list(CLIENTS)
    unknown = [n for n in names if n not in CLIENTS]
    if unknown:
        print(f"unknown client(s): {unknown}; known: {list(CLIENTS)}", file=sys.stderr)
        return 2
    results = [check(n, a.repo, a.timeout) for n in names]
    cols = ["client", "config", "spawn ok", "tools", "run ok", "fs ok", "hook", "ms"]
    rows = [[r.client, r.config, r.spawn, r.tools, r.run, r.fs, r.hook, str(r.ms)] for r in results]
    widths = [max(len(c), *(len(row[i]) for row in rows)) for i, c in enumerate(cols)]
    for line in [cols, ["-" * w for w in widths], *rows]:
        print("| " + " | ".join(c.ljust(w) for c, w in zip(line, widths)) + " |")
    if NOTES:
        print("\nNOTES")
        for n in NOTES:
            print(" -", n)
    bad = [f for r in results for f in [f"{r.client}: {x}" for x in r.failures]]
    if bad:
        print("\nFAILURES")
        for f in bad:
            print(" -", f)
        return 1
    print("\nall clients ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
