"""gold_eval: what qx cuts from omp's built-in tool results, weighted by assumed omp session share.

usage: python scripts/gold_eval.py [--json]
  QX_EXE=<abs path>   qx binary (default ~/.cargo/bin/qx.exe)
Imports scripts/vs_rtk.py for fixtures, cases(), token count and fact scoring.
Tools, measured as the omp extension (integrations/omp/qx.js) acts on them:
  bash: each vs_rtk case's raw output piped through `qx squeeze --label <cmd> --code <exit code> --max-ratio 70`;
        the raw output stays when the digest is not shorter.
  data reads: fixtures; raw = omp's capped `N:line` read; only results over 6000 chars are digested (`qx cat`),
        and the original stays unless the digest is under 70% of it.
Retention counts only the facts present in the raw text: qx is scored against what omp would have shown.
omp's own grep and glob output is untouched by qx, so it is not scored.
Exit 0 when weighted saving >= 40% and weighted fact retention >= 95%, else 1.
"""
import argparse, json, os, subprocess, sys
from pathlib import Path

import vs_rtk as vs

GOLD = Path(os.environ.get("LOCALAPPDATA", str(Path.home()))) / "Temp" / "gold_eval"
# Assumed shares of omp tool-result volume, from a one-time sample of omp sessions; not re-measured here.
WEIGHTS = {"bash": 9.0, "data reads": 2.3}
READ_LIMIT = 300  # omp read.defaultLimit
DATA_MIN_CHARS, DATA_MAX_RATIO = 6000, 0.7  # the extension's data-read rule
MIN_SAVING, MIN_FACTS = 40.0, 95.0


def count(t):
    return vs.tik(t) if vs._enc else vs.tok(t)


def row(name, raw, out, facts):
    """Tokens before/after and facts kept, out of the facts the raw text itself holds."""
    present = [f for f in facts if vs.score([f], raw)[0]]
    return dict(name=name, raw=count(raw), qx=count(out), total=len(present), kept=vs.score(present, out)[0])


def run_case(argv, cwd):
    """Like vs.run_proc (stdout + stderr), plus the exit code; 0 only when the run itself failed to start."""
    exe = vs.shutil.which(argv[0], path=vs.ENV_SH["PATH"]) or argv[0]
    try:
        r = subprocess.run([exe] + argv[1:], cwd=cwd, env=vs.ENV_SH, capture_output=True, timeout=vs.TIMEOUT)
        return r.stdout.decode("utf-8", "replace") + r.stderr.decode("utf-8", "replace"), r.returncode
    except Exception as e:  # missing exe, timeout
        return f"<<harness: {type(e).__name__}: {e}>>", 0


def squeeze(raw, label, code):
    """What omp's tool_result handler leaves: the `qx squeeze` digest when shorter, else the raw output."""
    try:
        r = subprocess.run([vs.QX, "squeeze", "--label", label, "--code", str(code), "--max-ratio", "70"],
                           input=raw.encode("utf-8"), env=vs.ENV, capture_output=True, timeout=vs.TIMEOUT)
    except Exception:
        return raw
    out = r.stdout.decode("utf-8", "replace").rstrip()
    return out if r.returncode == 0 and out and len(out) < len(raw.rstrip()) else raw


def bash_rows():
    """vs_rtk cases: raw tokens, tokens after `qx squeeze` (what omp's bash result becomes), facts kept."""
    if vs.FX.exists():
        vs.rmtree(vs.FX)
    vs.BASE.mkdir(parents=True, exist_ok=True)
    rows = []
    for c in vs.cases():
        pre = c.get("pre") or (lambda: None)
        pre()
        raw, code = run_case(c["argv"], c["cwd"])
        facts = c["facts"](raw) if callable(c["facts"]) else c["facts"]
        out = squeeze(raw, " ".join(c["argv"]), code)
        rows.append(row(c["name"], raw, out, facts))
        print(f"bash: {c['name']}", file=sys.stderr)
    return rows


def data_fixtures():
    d = GOLD / "fx"
    if d.exists():
        vs.rmtree(d)
    recs = [{"id": 5000 + i, "name": f"user-{i:04d}", "email": f"user-{i:04d}@example.test",
             "tags": ["a", "b"], "score": round(i * 0.37 + 1.01, 2), "active": i % 3 == 0} for i in range(300)]
    vs.w(d / "data.json", json.dumps(recs, indent=2))
    jf = []
    for i in (0, 150, 299):
        jf += [vs.F(f"id {5000 + i}", f"re:\\b{5000 + i}\\b"), vs.F(f"email {i}", f"user-{i:04d}@example.test")]
    errs = {150: "db pool exhausted shard=3", 480: "upstream timeout after 30s host=api-7",
            900: "checksum mismatch block=88213", 1300: "disk quota exceeded vol=/data2",
            1650: "tls handshake failed peer=10.0.4.17", 1980: "worker crashed code=139 pid=4411"}
    log = [f"2026-01-01T{i // 3600 % 24:02}:{i // 60 % 60:02}:{i % 60:02}Z INFO worker-{i % 8} handled request "
           f"id={100000 + i} status=200 ms={i % 97}" for i in range(2000)]
    for n, msg in errs.items():
        log[n] = f"2026-01-01T00:00:{n % 60:02}Z ERROR worker-{n % 8} {msg}"
    vs.w(d / "app.log", "\n".join(log) + "\n")
    rows = [f"{1000 + i},name-{i:03d},person{i:03d}@example.test,{i * 7 % 1000},City{i % 25}" for i in range(500)]
    vs.w(d / "table.csv", "id,name,email,score,city\n" + "\n".join(rows) + "\n")
    cf = [vs.F("header", "id,name,email,score,city")]
    for i in (0, 499):
        cf += [vs.F(f"row {i} id", f"{1000 + i}"), vs.F(f"row {i} name", f"name-{i:03d}"),
               vs.F(f"row {i} email", f"person{i:03d}@example.test")]
    return d, [("data.json", jf), ("app.log", [vs.F(f"ERROR {m}", m) for m in errs.values()]), ("table.csv", cf)]


def data_rows():
    d, files = data_fixtures()
    rows = []
    for name, facts in files:
        text = (d / name).read_text(encoding="utf-8").split("\n")
        raw = "\n".join(f"{i + 1}:{l}" for i, l in enumerate(text[:READ_LIMIT]))
        out = raw
        if len(raw) > DATA_MIN_CHARS:
            digest, _ = vs.run_proc([vs.QX, "cat", name], d)
            digest = digest.strip()
            footer = f"[qx] digest of {name}; read {name} with a line range ({name}:N-M) for exact text"
            if digest and len(digest) < len(raw) * DATA_MAX_RATIO:
                out = f"{digest}\n{footer}"
        rows.append(row(f"read {name}", raw, out, facts))
        print(f"data: {name}", file=sys.stderr)
    return rows


def pct(a, b):
    return 100.0 * a / b if b else 0.0


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--json", action="store_true", help=f"write results to {GOLD / 'results.json'}")
    a = ap.parse_args()

    groups = {"bash": bash_rows(), "data reads": data_rows()}

    print("\n| tool | weight | samples | raw tok | qx tok | saving % | facts kept |")
    print("|---|--:|--:|--:|--:|--:|--:|")
    summ, tr, tq, tk, tt = {}, 0, 0, 0, 0
    for tool, rows in groups.items():
        r, q = sum(x["raw"] for x in rows), sum(x["qx"] for x in rows)
        k, t = sum(x["kept"] for x in rows), sum(x["total"] for x in rows)
        summ[tool] = dict(weight=WEIGHTS[tool], samples=len(rows), raw=r, qx=q, saving=100 - pct(q, r) if r else None,
                          kept=k, total=t, retention=pct(k, t) if t else 100.0)
        s = summ[tool]
        sv = "n/a" if r == 0 else format(s["saving"], ".1f")
        print(f"| {tool} | {s['weight']} | {len(rows)} | {r} | {q} | {sv} | {k}/{t} |")
        tr, tq, tk, tt = tr + r, tq + q, tk + k, tt + t
    live = {t: s for t, s in summ.items() if s["raw"]}
    wsum = sum(s["weight"] for s in live.values())
    wsave = sum(s["weight"] * s["saving"] for s in live.values()) / wsum if wsum else 0.0
    wfacts = sum(s["weight"] * s["retention"] for s in live.values()) / wsum if wsum else 0.0
    print(f"\nunweighted: raw {tr} tok, qx {tq} tok, saving {100 - pct(tq, tr) if tr else 0:.1f}%, facts {tk}/{tt}")
    print(f"weighted:   saving {wsave:.1f}% (gate >= {MIN_SAVING:.0f}), facts {wfacts:.1f}% (gate >= {MIN_FACTS:.0f})")
    for t in summ.keys() - live.keys():
        print(f"WARN {t}: no samples, excluded from weights")
    for tool, rows in groups.items():
        for x in rows:
            if x["kept"] < x["total"]:
                print(f"missing facts: {tool} / {x['name']}: {x['kept']}/{x['total']}")
    print("code reads (report only): in a one-time sample of omp sessions about 87% of read chars were code "
          "(not measured here); qx leaves them untouched.")
    print(f"token count: {'tiktoken ' + vs._enc.name if vs._enc else 'bytes/4'}")

    ok = wsave >= MIN_SAVING and wfacts >= MIN_FACTS
    if a.json:
        GOLD.mkdir(parents=True, exist_ok=True)
        (GOLD / "results.json").write_text(json.dumps(dict(
            tools=summ, rows=groups, weighted_saving=wsave, weighted_facts=wfacts, ok=ok), indent=1), encoding="utf-8")
        print(f"results: {GOLD / 'results.json'}")
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
