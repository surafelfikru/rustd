#!/usr/bin/env python3
"""executed.py: how many of the functions rustd compiled for a program actually run.
Milestone 0 of the compile-on-first-call work, by the callgrind method.

    executed.py [SUBJECT...] [--work DIR] [--out FILE] [--keep]

For each subject of baseline.py: a fresh copy, one `cargo rustd build`, the compiled set read
from the daemon's object (`target/rustd/<key>/programs/<program>/rustd.o`: text symbols named
`rustd::...` are the instances the daemon made, the rest are roots), then the subject's session commands run from the
executable under callgrind with mangled names. A function counts as executed when callgrind
saw instructions in it. Reported per subject: compiled roots and instances, executed by the
first command alone and over the whole session, and the share of each. Test suites are not
run here. The executed names are kept in the JSON, since they are the warm list a lazy
build would start from.

Everything runs under `taskset -c 0-3`, as baseline.py does. Writes `executed.json` and
`EXECUTED.md` next to this file, or where `--out` says.
"""
import datetime, glob, json, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import baseline  # noqa: E402
from baseline import SUBJECTS, Side, environment, machine, pin_cores, prepare, wipe  # noqa: E402
from cargo_rustd import how_built  # noqa: E402

# The commands of one subject's session, each run from the executable in the workspace copy.
# `{work}` is replaced by the subject's scratch directory.
SESSIONS = {
    "hello": [[]],
    "ripgrep": [["fn main", "crates/core"], ["-n", "fn main", "crates"], ["-i", "FN MAIN", "crates"],
                ["--json", "fn main", "crates/core/main.rs"], ["-g", "*.rs", "fn main", "crates"]],
    "fd": [["--extension", "rs", "main", "src"], ["-t", "f", "-e", "toml", "."], ["-H", "-I", "--max-depth", "2", "src"]],
    "hyperfine": [["-N", "--runs", "3", "true"], ["-N", "--runs", "2", "--export-json", "{work}/hyperfine.json", "true"]],
    "bat": [["--plain", "--color=never", "README.md", "Cargo.toml"], ["-n", "--color=never", "README.md"]],
    "nushell": [["-n", "-c", "[3 1 2] | sort | str join ','"], ["-n", "-c", "'hello world' | str upcase"],
                ["-n", "-c", "ls | length"], ["-n", "-c", "[[a b]; [1 2] [3 4]] | to json"],
                ["-n", "-c", "open Cargo.toml | get package.name"]],
    "genws": [["5"]],
}
VALGRIND = ["valgrind", "--tool=callgrind", "--cache-sim=no", "--collect-jumps=no", "--demangle=no"]


def compiled_set(ws):
    """The functions in the daemon's object: instances under `rustd::` names, roots under the
    rest. The newest object under `target/rustd/` is the build's."""
    objects = glob.glob(os.path.join(ws, "target", "rustd", "*", "programs", "*", "rustd.o"))
    if not objects:
        raise SystemExit(f"no rustd.o under {ws}/target/rustd/*/programs: the daemon did not build")
    newest = max(objects, key=os.path.getmtime)
    listed = subprocess.run(["nm", "-f", "posix", "--defined-only", newest], capture_output=True, text=True, check=True)
    roots, instances = set(), set()
    for line in listed.stdout.splitlines():
        parts = line.split()
        if len(parts) < 2 or parts[1] not in ("T", "t"):
            continue
        name = parts[0]
        # The daemon names an instance it made for the program `rustd::<path>::<n>`; a root
        # keeps rustc's symbol, or `rustd.<crate id>.<path>` for a crate rustc built nothing of.
        (instances if name.startswith("rustd::") else roots).add(name)
    return newest, roots, instances


def executed_in(profile):
    """The mangled names of every function callgrind charged instructions to in `profile`.
    Names are compressed: `fn=(3) name` defines id 3, a later `fn=(3)` refers to it; `cfn=`
    lines define ids the same way."""
    names, by_id = set(), {}
    for line in open(profile, errors="replace"):
        if not (line.startswith("fn=") or line.startswith("cfn=")):
            continue
        match = re.match(r"c?fn=\((\d+)\)(?: (.*))?$", line.rstrip("\n"))
        if not match:
            continue
        ident, name = match.group(1), match.group(2)
        if name is not None:
            by_id[ident] = name
        if line.startswith("fn=") and ident in by_id:
            names.add(by_id[ident])
    return names


def run_session(name, exe, ws, env, work):
    """Each command of the subject's session under callgrind, from the workspace copy. The
    result is the executed names per command, in order."""
    per_command = []
    for at, args in enumerate(SESSIONS[name]):
        args = [a.replace("{work}", work) for a in args]
        profile = os.path.join(work, f"{name}-callgrind-{at}.out")
        ran = subprocess.run([*VALGRIND, f"--callgrind-out-file={profile}", exe, *args], cwd=ws, env=env,
                             capture_output=True, text=True, errors="replace", stdin=subprocess.DEVNULL)
        if ran.returncode not in (0, 1) and not os.path.exists(profile):
            raise SystemExit(f"{name}: callgrind failed on {args}: {ran.stderr[-1500:]}")
        per_command.append(dict(args=args, status=ran.returncode, names=executed_in(profile)))
        print(f"  {name} command {at}: exit {ran.returncode}, {len(per_command[-1]['names'])} functions ran", flush=True)
    return per_command


def measure(name, work):
    spec = SUBJECTS[name]
    env = environment(work, name)
    ws, _ = prepare(name, spec, work, env)
    ours = Side(ws, env, True)
    ours.cargo("stop")
    took, ran = ours.cargo("build")
    built = how_built(ran.stderr)
    if ran.returncode != 0 or built != "daemon":
        raise SystemExit(f"{name}: the daemon did not build it ({built}): {ran.stderr[-1500:]}")
    print(f"  {name}: built by the daemon in {took:.1f}s", flush=True)
    obj, roots, instances = compiled_set(ws)
    exe = os.path.join(ws, "target", "debug", spec["bin"])
    commands = run_session(name, exe, ws, env, work)
    ours.cargo("stop")
    compiled = roots | instances
    first = commands[0]["names"] & compiled
    session = set().union(*(c["names"] for c in commands)) & compiled

    def split(found):
        return dict(roots=len(found & roots), instances=len(found & instances), total=len(found),
                    share=round(len(found) / max(len(compiled), 1), 4))

    return dict(object=obj, compiled=dict(roots=len(roots), instances=len(instances), total=len(compiled)),
                first=split(first), session=split(session),
                commands=[dict(args=c["args"], status=c["status"], executed=len(c["names"] & compiled)) for c in commands],
                executed_names=sorted(session), build_s=round(took, 3), **machine())


def table(result):
    out = ["# Executed share: functions rustd compiled against functions that ran", "",
           f"Measured {result['date']} by the callgrind method (`harness/executed.py`), one cold `cargo rustd build` per subject, "
           "then the session commands under valgrind. A function counts as executed when callgrind charged it any instruction. "
           "Test suites are not included.", "",
           "| Subject | compiled roots (+instances) | ran, first command | ran, whole session | share, first | share, session |",
           "|---|---:|---:|---:|---:|---:|"]
    for name, r in result["results"].items():
        if "compiled" not in r:
            out.append(f"| {name} | failed: {r.get('error', '')[:120]} | | | | |")
            continue
        c, f, s = r["compiled"], r["first"], r["session"]
        out.append(f"| {name} | {c['roots']:,} (+{c['instances']:,}) | {f['roots']:,} (+{f['instances']:,}) | {s['roots']:,} (+{s['instances']:,}) | "
                   f"{100 * f['share']:.1f} % | {100 * s['share']:.1f} % |")
    out.append("")
    return "\n".join(out)


def main():
    pin_cores()
    argv = sys.argv[1:]
    work = os.path.expanduser((baseline.bench.options(argv, "--work") or [None])[0] or "~/rustd-bench/executed")
    out = baseline.bench.options(argv, "--out", os.path.join(HERE, "executed.json"))[0]
    keep = "--keep" in argv
    names = [a for a in argv if not a.startswith("--")] or [n for n in SUBJECTS if n in SESSIONS]
    unknown = [n for n in names if n not in SESSIONS]
    if unknown:
        raise SystemExit(f"no session for {unknown}; subjects with one: {list(SESSIONS)}")
    if shutil.which("valgrind") is None:
        raise SystemExit("valgrind is not installed")
    os.makedirs(work, exist_ok=True)
    result = dict(date=datetime.datetime.now().astimezone().isoformat(timespec="seconds"), results={})
    if os.path.exists(out):
        result = json.load(open(out))
        result["date"] = datetime.datetime.now().astimezone().isoformat(timespec="seconds")
    for name in names:
        print(f"{name}: work {work}", flush=True)
        try:
            result["results"][name] = measure(name, work)
            r = result["results"][name]
            print(f"  {name}: compiled {r['compiled']['total']:,}, ran first {r['first']['total']:,} ({100 * r['first']['share']:.1f} %), "
                  f"session {r['session']['total']:,} ({100 * r['session']['share']:.1f} %)", flush=True)
        except Exception as why:  # one subject failing must not lose the others
            print(f"  FAILED {name}: {why!r}", flush=True)
            result["results"][name] = dict(error=repr(why))
        json.dump(result, open(out, "w"), indent=1)
        open(os.path.join(os.path.dirname(out), "EXECUTED.md"), "w").write(table(result))
        if not keep:
            for entry in os.listdir(work):
                if entry in (name, f"home-{name}"):
                    wipe(os.path.join(work, entry))
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
