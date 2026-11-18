"""qx vs rtk vs raw: tokens and facts kept, per command, measured on what an agent sees.

usage: python scripts/vs_rtk.py            (stdlib only, Python 3.12)
  QX_EXE=<abs path>   qx binary (default ~/.cargo/bin/qx.exe)
  VSRTK_ONLY=a,b      run only cases whose name contains one of these words

Sides per case (each starts from the same fixture state):
  raw  the bare command, stdout+stderr
  rtk  `rtk rewrite "<cmd>"` (what the rtk hook turns it into), run directly, stdout+stderr;
       commands rtk does not rewrite run as-is (rtk gives no compaction there)
  qx   the same command through the MCP `run` tool (default mode, exactly the reply an agent gets)
Fixtures live under %LOCALAPPDATA%/Temp/vsrtk/ (never inside the repo: target/ and scratch/ carry a
Low integrity label). Results: results.json there; dumps <case>.{raw,rtk,qx}.txt for every case where
qx loses a fact, is bigger than rtk, or rtk drops a fact.
Facts are things an agent needs (names, messages, codes, file:line). A fact counts as kept when the
output contains it (alternatives allowed: rtk/qx may format a location differently). Facts are not
checked for grouping (e.g. staged vs unstaged) unless the fact says so.
"""
import json, os, re, shlex, shutil, stat, subprocess, sys, time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BASE = Path(os.environ.get("LOCALAPPDATA", str(Path.home()))) / "Temp" / "vsrtk"
FX = BASE / "fx"
QX = os.environ.get("QX_EXE") or str(Path.home() / ".cargo" / "bin" / "qx.exe")
RTK = shutil.which("rtk") or str(Path.home() / ".cargo" / "bin" / "rtk.exe")
ENV = dict(os.environ, CARGO_TARGET_DIR=str(BASE / "cargo-target"), GIT_TERMINAL_PROMPT="0",
           QX_STATS="off", QX_RUN_MEMORY="off", QX_WARM="off")
# Git Bash agents see Git usr\bin before System32 (find/sort); give raw and rtk the same PATH.
GITBIN = r"C:\Program Files\Git\usr\bin"
ENV_SH = dict(ENV, PATH=GITBIN + os.pathsep + ENV.get("PATH", ""))
TIMEOUT = 240


# ---------- scoring ----------
def norm(t):
    t = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", t)
    return t.replace("\r\n", "\n").replace("\\", "/")


def F(label, *alts):
    """A fact. alt: plain substring, 're:<regex>', or callable(norm_output)->bool. Any alt keeps it."""
    return (label, alts or (label,))


def kept(fact, out):
    for a in fact[1]:
        if callable(a):
            if a(out):
                return True
        elif a.startswith("re:"):
            if re.search(a[3:], out, re.M):
                return True
        elif a in out:
            return True
    return False


def loc(file, line):
    base = re.escape(file.rsplit("/", 1)[-1])
    return F(f"{file}:{line}", f"{file}:{line}", f"re:{base}[^\\n]{{0,4}}\\b{line}\\b",
             f"re:(?s){base}.{{0,120}}\\bL?{line}[:\\s]")


_BRACE = re.compile(r"""([^\s{}"':]*)\{([^{}\s"':]*,[^{}\s"':]*)\}([^\s{}"':]*)""")


def expand_braces(t):
    """`src/{a,b}.rs` -> `src/a.rs src/b.rs`: a lossless shell-brace listing counts as naming each path."""
    while True:
        n = _BRACE.sub(lambda m: " ".join(m[1] + x + m[3] for x in m[2].split(",")), t)
        if n == t:
            return t
        t = n


def score(facts, out):
    o = norm(out)
    o = o + "\n" + expand_braces(o)  # expansion may only add matches, never hide a literal `{a,b}`
    miss = [f[0] for f in facts if not kept(f, o)]
    return len(facts) - len(miss), miss


_enc = None
try:
    import tiktoken
    for name in ("o200k_base", "cl100k_base"):
        try:
            _enc = tiktoken.get_encoding(name)
            break
        except Exception:
            pass
except Exception:
    pass


def tok(t):
    return (len(t.encode("utf-8")) + 3) // 4


def tik(t):
    return len(_enc.encode(t, disallowed_special=())) if _enc else None


# ---------- running ----------
def run_proc(argv, cwd):
    exe = shutil.which(argv[0], path=ENV_SH["PATH"]) or argv[0]
    t = time.perf_counter()
    try:
        r = subprocess.run([exe] + argv[1:], cwd=cwd, env=ENV_SH, capture_output=True, timeout=TIMEOUT)
        out = r.stdout.decode("utf-8", "replace") + r.stderr.decode("utf-8", "replace")
    except Exception as e:  # missing exe, timeout
        out = f"<<harness: {type(e).__name__}: {e}>>"
    return out, time.perf_counter() - t


def rtk_argv(argv, cwd):
    line = shlex.join(argv)
    r = subprocess.run([RTK, "rewrite", line], cwd=cwd, env=ENV_SH, capture_output=True, text=True)
    # exit 0 = rewritten, 3 = rewritten but hook would ask, 1 = no rtk equivalent
    if r.returncode not in (0, 3) or not r.stdout.strip():
        return argv, False
    return shlex.split(r.stdout.strip()), True


class Qx:
    def __init__(self):
        self.p = subprocess.Popen([QX, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, env=ENV)  # default buffering
        self.n = 0
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "claude-code", "version": "2"}})
        self.rpc("notifications/initialized", notify=True)

    def rpc(self, method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if not notify:
            self.n += 1
            msg["id"] = self.n
        self.p.stdin.write((json.dumps(msg) + "\n").encode())
        self.p.stdin.flush()
        if notify:
            return None
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("qx mcp closed stdout")
            r = json.loads(line)
            if r.get("id") == self.n:
                return r

    def run(self, argv, cwd):
        t = time.perf_counter()
        r = self.rpc("tools/call", {"name": "run", "arguments": {
            "program": argv[0], "args": argv[1:], "cwd": str(cwd).replace("\\", "/"),
            "timeout_ms": TIMEOUT * 1000}})
        res = r.get("result") or {"content": [{"text": json.dumps(r.get("error"))}]}
        out = "".join(c.get("text", "") for c in res.get("content", []))
        if res.get("structuredContent") is not None:
            out += json.dumps(res["structuredContent"])
        return out, time.perf_counter() - t

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()


# ---------- fixtures ----------
def rmtree(p):
    def fix(fn, path, _):
        os.chmod(path, stat.S_IWRITE)
        fn(path)
    shutil.rmtree(p, onerror=fix)


def w(path, text):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")


def sh(cwd, *argv):
    r = subprocess.run(list(argv), cwd=cwd, env=ENV, capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"{argv} failed: {r.stdout}{r.stderr}")
    return r.stdout


def git(cwd, *a):
    return sh(cwd, "git", "-c", "core.autocrlf=false", "-c", "user.name=T", "-c",
              "user.email=t@example.test", *a)


def body(name, special=None, n=40):
    lines = [f"# {name} line {i}" for i in range(1, n + 1)]
    for k, v in (special or {}).items():
        lines[k - 1] = v
    return "\n".join(lines) + "\n"


TOPICS = ["parser", "cache", "retry", "router", "config", "logger", "scheduler", "codec", "auth",
          "metrics", "queue"]


def make_git(d):
    d.mkdir(parents=True)
    git(d, "init", "-q", "-b", "main")
    w(d / "README.md", body("README", {6: "retries = 3"}))
    w(d / "docs/guide.md", body("guide", {6: "timeout = 10", 20: "flaky = yes"}))
    w(d / "src/core.py", body("core", {10: "mode = fast"}))
    w(d / "src/util.py", body("util", {6: "level = 1"}))
    w(d / "src/legacy.py", body("legacy"))
    w(d / "old.txt", body("old"))
    git(d, "add", "-A")
    git(d, "commit", "-q", "-m", "chore: initial import")
    for i in range(1, 23):
        t = TOPICS[i % len(TOPICS)]
        w(d / f"src/mod_{i:02}.py", body(f"mod_{i:02}", n=8))
        git(d, "add", "-A")
        msg = ["-m", f"feat(mod{i:02}): add {t} handler #{i}"]
        if i % 4 == 0:
            msg += ["-m", f"Motivation {i}: the {t} path was racy.\nSecond body line {i}."]
        git(d, "commit", "-q", *msg)
    # head commit for `git show`: 2 files + body
    w(d / "src/core.py", body("core", {10: "mode = safe"}))
    w(d / "docs/guide.md", body("guide", {6: "timeout = 10", 20: "flaky = no"}))
    git(d, "add", "-A")
    git(d, "commit", "-q", "-m", "fix(core): switch to safe mode", "-m",
        "Body: fast mode corrupted the cache under load.")
    # worktree: staged modify/new/rename, staged+unstaged, unstaged modify, deleted, untracked
    w(d / "src/core.py", body("core", {10: "mode = safe", 11: "extra = 1"}))
    w(d / "src/new_feature.py", body("new_feature", n=5))
    git(d, "mv", "src/legacy.py", "src/renamed.py")
    w(d / "src/util.py", body("util", {6: "level = 2"}))
    git(d, "add", "src/core.py", "src/new_feature.py", "src/util.py")
    w(d / "src/util.py", body("util", {6: "level = 3"}))
    w(d / "README.md", body("README", {6: "retries = 5"}))
    w(d / "docs/guide.md", body("guide", {6: "timeout = 30", 20: "flaky = no"}))
    os.remove(d / "old.txt")
    w(d / "notes.tmp", "scratch notes\n")
    w(d / "build.log", "log\n")
    w(d / "scratch/idea.txt", "idea\n")
    hs = git(d, "log", "-n", "20", "--format=%h\t%s").strip().split("\n")
    return [tuple(x.split("\t", 1)) for x in hs]


def make_cargo(root):
    py = {}
    ok = root / "okcrate"
    src = ("pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n#[cfg(test)]\nmod tests {\n"
           "    use super::*;\n")
    for i in range(6):
        src += f"\n    #[test]\n    fn adds_ok_{i}() {{\n        assert_eq!(add({i}, 1), {i + 1});\n    }}\n"
    src += ("\n    #[test]\n    fn adds_broken() {\n        assert_eq!(add(2, 2), 5, \"math is hard\");\n"
            "    }\n}\n")
    py["ok"] = src.split("\n").index('        assert_eq!(add(2, 2), 5, "math is hard");') + 1
    toml = '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n'
    w(root / "okcrate/Cargo.toml", toml % "okcrate")
    w(ok / "src/lib.rs", src)
    err = ("pub fn parse(s: &str) -> i32 {\n    let n: i32 = s;\n    n + 1\n}\n\n"
           "pub fn run() -> i32 {\n    undefined_fn(3)\n}\n")
    w(root / "errcrate/Cargo.toml", toml % "errcrate")
    w(root / "errcrate/src/lib.rs", err)
    py["err1"], py["err2"] = 2, 7
    lint = ("pub fn first(v: &Vec<i32>) -> i32 {\n    if v.len() == 0 {\n        return 0;\n    }\n"
            "    return v[0];\n}\n")
    w(root / "lintcrate/Cargo.toml", toml % "lintcrate")
    w(root / "lintcrate/src/lib.rs", lint)
    py["l1"], py["l2"], py["l3"] = 1, 2, 5
    return py


def touch(p):
    def f():
        os.utime(p, None)
    return f


def have(cmd):
    return shutil.which(cmd) is not None


def sample(seq, k):
    if len(seq) <= k:
        return list(seq)
    return [seq[i * (len(seq) - 1) // (k - 1)] for i in range(k)]


def grep_facts(token, k=15):
    def build(raw):
        rows = []
        for l in norm(raw).split("\n"):
            m = re.match(r"^([^:\n]+):(\d+):(.*)$", l)
            if m:
                rows.append((m.group(1), m.group(3).strip()))
        n = len(rows)

        def win(c):
            i = c.find(token)
            return c[max(0, i - 10): i + len(token) + 10]
        cnt = {}
        for f, c in rows:
            cnt[win(c)] = cnt.get(win(c), 0) + 1
        cand = [(f, win(c)) for f, c in rows if cnt[win(c)] == 1 and len(win(c)) >= 12]
        facts = [F(f"match count = {n}",
                   lambda o, n=n: len(re.findall(r"^[^\n:]+:\d+:", o, re.M)) >= n,
                   f"re:\\b{n}\\b")]
        for f, c in sample(cand, k):
            base = f.rsplit("/", 1)[-1]
            facts.append(F(f"{f}: {c}", lambda o, b=base, c=c: b in o and c in o))
        return facts
    return build


def path_facts(k=30):
    def build(raw):
        lines = [l.strip() for l in norm(raw).split("\n") if l.strip()]
        names = [l.rsplit("/", 1)[-1] for l in lines]
        cnt = {}
        for n in names:
            cnt[n] = cnt.get(n, 0) + 1
        uniq = [n for n in names if cnt[n] == 1]
        return [F(n) for n in sample(uniq, k)]
    return build


def cases():
    out = []
    FX.mkdir(parents=True, exist_ok=True)

    # git
    g = FX / "git"
    hashes = make_git(g)
    st = ([F("branch main", "main"), F("untracked marker", "re:(?i)untracked|\\?\\?"),
           F("deleted old.txt", "re:(?is)(deleted|\\bD\\b).{0,40}old\\.txt|old\\.txt.{0,40}(deleted|\\bD\\b)")]
          + [F(n) for n in ["README.md", "guide.md", "core.py", "util.py", "new_feature.py", "renamed.py",
                            "legacy.py", "old.txt", "notes.tmp", "build.log", "scratch/"]])
    out.append(dict(name="git status", cwd=g, argv=["git", "status"], facts=st))
    lg = [F(f"subject: {s}", s) for _, s in hashes] + [F(f"hash {hashes[i][0]}", hashes[i][0]) for i in (0, 4, 9, 14, 19)]
    out.append(dict(name="git log -n 20", cwd=g, argv=["git", "log", "-n", "20"], facts=lg))
    df = [F(n) for n in ["README.md", "guide.md", "util.py", "old.txt"]] + [
        F("+retries = 5"), F("-retries = 3"), F("+timeout = 30"), F("-timeout = 10"),
        F("+level = 3"), F("-level = 2")]
    out.append(dict(name="git diff (4 files)", cwd=g, argv=["git", "diff"], facts=df))
    sw = [F("subject: fix(core): switch to safe mode", "fix(core): switch to safe mode"),
          F("hash " + hashes[0][0], hashes[0][0]), F("core.py"), F("guide.md"),
          F("+mode = safe"), F("-mode = fast"), F("+flaky = no"), F("-flaky = yes"),
          F("body line", "fast mode corrupted the cache under load")]
    out.append(dict(name="git show", cwd=g, argv=["git", "show"], facts=sw))

    # cargo
    if have("cargo"):
        cr = FX / "cargo"
        c = make_cargo(cr)
        lr = lambda crate: touch(cr / crate / "src/lib.rs")
        out.append(dict(name="cargo test (1 fail/6 pass)", cwd=cr / "okcrate", argv=["cargo", "test", "-j", "1"],
                        pre=lr("okcrate"), facts=[
                            F("adds_broken"), F("math is hard"), F("left: 4"), F("right: 5"),
                            loc("src/lib.rs", c["ok"]), F("1 failed", "re:\\b1 failed"),
                            F("6 passed", "re:\\b6 passed")]))
        out.append(dict(name="cargo build (E0308,E0425)", cwd=cr / "errcrate", argv=["cargo", "build", "-j", "1"],
                        pre=lr("errcrate"), facts=[
                            F("E0308"), F("E0425"), F("mismatched types"),
                            F("cannot find function `undefined_fn`"),
                            loc("src/lib.rs", c["err1"]), loc("src/lib.rs", c["err2"]),
                            F("could not compile")]))
        if sh(cr, "cargo", "clippy", "--version"):
            out.append(dict(name="cargo clippy (3 warns)", cwd=cr / "lintcrate", argv=["cargo", "clippy", "-j", "1"],
                            pre=lr("lintcrate"), facts=[
                                F("clippy::ptr_arg", "re:ptr_arg"), F("clippy::len_zero", "re:len_zero"),
                                F("clippy::needless_return", "re:needless_return"),
                                loc("src/lib.rs", c["l1"]), loc("src/lib.rs", c["l2"]), loc("src/lib.rs", c["l3"])]))

    # python
    if have("pytest"):
        p = FX / "py"
        t = "from calc import total\n\n"
        for i in range(23):
            t += f"\ndef test_ok_{i}():\n    assert total([{i}, 1]) == {i + 1}\n"
        t += "\n\ndef test_total_bad():\n    assert total([1, 2, 3]) == 7\n"
        w(p / "calc.py", "def total(xs):\n    return sum(xs)\n")
        w(p / "test_calc.py", t)
        ln = t.split("\n").index("    assert total([1, 2, 3]) == 7") + 1
        out.append(dict(name="pytest (1 fail/23 pass)", cwd=p, argv=["pytest", "-p", "no:cacheprovider"], facts=[
            F("test_total_bad"), F("assert 6 == 7"), F("total([1, 2, 3])"), loc("test_calc.py", ln),
            F("1 failed", "re:\\b1 failed"), F("23 passed", "re:\\b23 passed")]))
    a = FX / "py2"
    w(a / "app.py", "def load(cfg):\n    return get_port(cfg)\n\n\ndef get_port(cfg):\n"
      "    raise ValueError(\"bad config key: port\")\n\n\ndef main():\n    load({})\n\n\nmain()\n")
    out.append(dict(name="python traceback", cwd=a, argv=["python", "app.py"], facts=[
        F("ValueError"), F("bad config key: port"), F("app.py"), F("line 2", "re:line 2\\b"),
        F("line 6", "re:line 6\\b"), F("line 10", "re:line 10\\b"), F("line 13", "re:line 13\\b"),
        F("in get_port"), F("in load"), F("in main")]))

    # listings
    out.append(dict(name="grep -rn Result src", cwd=REPO, argv=["grep", "-rn", "Result", "src"],
                    facts=grep_facts("Result")))
    ld = FX / "lsdir"
    names = [f"mod_{i:02}.rs" for i in range(30)]
    for i, n in enumerate(names):
        w(ld / n, "x" * (100 + 997 * i))
    w(ld / ".gitignore", "target\n")
    for sub in ("docs", "tests", "benches"):
        (ld / sub).mkdir()
    big = 100 + 997 * 29
    out.append(dict(name="ls -la (34 entries)", cwd=ld, argv=["ls", "-la"], facts=[F(n) for n in names] + [
        F(".gitignore"), F("docs"), F("tests"), F("benches"),
        F(f"size {big}", str(big), f"{big / 1024:.1f}K", f"{big / 1024:.1f}KB", f"{big / 1024:.0f}K")]))
    out.append(dict(name="find src -name *.rs", cwd=REPO, argv=["find", "src", "-name", "*.rs"],
                    facts=path_facts(40)))
    out.append(dict(name="git ls-files (repo)", cwd=REPO, argv=["git", "ls-files"], facts=path_facts(40)))
    recs = [{"id": i, "name": f"user-{i:04d}", "email": f"user-{i:04d}@example.test",
             "tags": ["a", "b", "c"], "address": {"zip": f"Z{i:04d}-{(i * 37) % 9973:04d}", "city": "Springfield"},
             "score": round(i * 0.37 + 1.01, 2), "active": i % 3 == 0} for i in range(300)]
    jd = FX / "json"
    w(jd / "data.json", json.dumps({"items": recs}, indent=2))
    jf = []
    for i in (7, 150, 299):
        jf += [F(f"email {i}", f"user-{i:04d}@example.test"), F(f"zip {i}", f"Z{i:04d}-{(i * 37) % 9973:04d}")]
    out.append(dict(name="cat data.json (300 recs)", cwd=jd, argv=["cat", "data.json"], facts=jf))

    # node
    if have("tsc"):
        n = FX / "ts"
        w(n / "tsconfig.json", json.dumps({"compilerOptions": {"strict": True, "noEmit": True, "target": "es2020",
                                                               "module": "commonjs", "types": []},
                                           "include": ["index.ts"]}))
        w(n / "index.ts", "export function area(w: number, h: number): number {\n  return w * h;\n}\n"
          "const x: number = \"five\";\narea(1, \"2\");\n")
        out.append(dict(name="tsc (TS2322,TS2345)", cwd=n, argv=["tsc", "-p", "."], facts=[
            F("TS2322"), F("TS2345"), F("not assignable to type 'number'"),
            F("parameter of type 'number'"), loc("index.ts", 4), loc("index.ts", 5)]))
    return out


def slug(s):
    return re.sub(r"[^A-Za-z0-9]+", "_", s).strip("_")


def main():
    if FX.exists():
        rmtree(FX)
    BASE.mkdir(parents=True, exist_ok=True)
    only = [x for x in os.environ.get("VSRTK_ONLY", "").split(",") if x]
    cs = [c for c in cases() if not only or any(o in c["name"] for o in only)]
    qx = Qx()
    rows = []
    try:
        for c in cs:
            pre = c.get("pre") or (lambda: None)
            pre()
            raw, t_raw = run_proc(c["argv"], c["cwd"])
            facts = c["facts"](raw) if callable(c["facts"]) else c["facts"]
            pre()
            rargv, rewritten = rtk_argv(c["argv"], c["cwd"])
            rtk, t_rtk = run_proc(rargv, c["cwd"])
            pre()
            qxo, t_qx = qx.run(c["argv"], c["cwd"])
            r = dict(case=c["name"], rtk_cmd=shlex.join(rargv), rtk_rewritten=rewritten, facts_total=len(facts))
            for k, o, t in (("raw", raw, t_raw), ("rtk", rtk, t_rtk), ("qx", qxo, t_qx)):
                nk, miss = score(facts, o)
                r[k] = dict(tok=tok(o), tiktoken=tik(o), bytes=len(o.encode()), facts=nk, missing=miss,
                            secs=round(t, 2))
            rows.append(r)
            flags = []
            if r["raw"]["facts"] < len(facts):
                flags.append("raw-missing-facts:" + "|".join(r["raw"]["missing"]))
            lose = (r["qx"]["facts"] < len(facts) or r["qx"]["tok"] > r["rtk"]["tok"]
                    or r["rtk"]["facts"] < len(facts))
            if lose:
                stem = BASE / slug(c["name"])
                for k, o in (("raw", raw), ("rtk", rtk), ("qx", qxo)):
                    Path(f"{stem}.{k}.txt").write_text(o, encoding="utf-8", newline="")
                r["dump"] = str(stem) + ".{raw,rtk,qx}.txt"
            if flags:
                r["warn"] = flags
            print(f"done {c['name']}: raw {r['raw']['tok']} rtk {r['rtk']['tok']} qx {r['qx']['tok']}",
                  file=sys.stderr)
    finally:
        qx.close()
    report(rows)


def report(rows):
    tot = lambda k, f: sum(r[k][f] or 0 for r in rows)
    N = sum(r["facts_total"] for r in rows)
    print("\n| case | raw tok | rtk tok | qx tok | rtk facts | qx facts |")
    print("|---|--:|--:|--:|--:|--:|")
    for r in rows:
        print(f"| {r['case']} | {r['raw']['tok']} | {r['rtk']['tok']} | {r['qx']['tok']} | "
              f"{r['rtk']['facts']}/{r['facts_total']} | {r['qx']['facts']}/{r['facts_total']} |")
    print(f"| **total** | {tot('raw', 'tok')} | {tot('rtk', 'tok')} | {tot('qx', 'tok')} | "
          f"{tot('rtk', 'facts')}/{N} | {tot('qx', 'facts')}/{N} |")
    if _enc:
        print(f"\ntiktoken ({_enc.name}) totals: raw {tot('raw', 'tiktoken')}, rtk {tot('rtk', 'tiktoken')}, "
              f"qx {tot('qx', 'tiktoken')}")
    print("\nTokens = ceil(utf8 bytes / 4).")
    print("\nqx loses to rtk (more tokens, or fewer facts than rtk) / qx lost a fact:")
    for r in rows:
        why = []
        if r["qx"]["tok"] > r["rtk"]["tok"]:
            why.append(f"tokens {r['qx']['tok']} > {r['rtk']['tok']}")
        if r["qx"]["facts"] < r["rtk"]["facts"]:
            why.append(f"facts {r['qx']['facts']} < rtk {r['rtk']['facts']}")
        if r["qx"]["missing"]:
            why.append("qx missing: " + "; ".join(r["qx"]["missing"]))
        if why:
            print(f"- {r['case']}: {' | '.join(why)}  -> {r.get('dump', '')}")
    print("\nrtk drops facts:")
    for r in rows:
        if r["rtk"]["missing"]:
            print(f"- {r['case']} ({r['rtk_cmd']}): " + "; ".join(r["rtk"]["missing"]))
    for r in rows:
        if r.get("warn"):
            print(f"WARN {r['case']}: {r['warn']}")
    (BASE / "results.json").write_text(json.dumps(rows, indent=1), encoding="utf-8")
    print(f"\nresults: {BASE / 'results.json'}")


if __name__ == "__main__":
    main()
