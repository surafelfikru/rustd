#!/usr/bin/env python3
"""baseline.py: the first build through `cargo rustd` against plain cargo, phase by phase,
repeated, on real projects. Stage 1 of the compile-on-first-call work: the numbers a change
to the first build is judged against.

    baseline.py [SUBJECT...] [--runs N] [--events EVENT,...] [--work DIR] [--out FILE] [--keep]

Subjects: hello, ripgrep, fd, hyperfine, bat, nushell, cargo, genws (100 crates). Each real
project is cloned at its pinned tag into `RUSTD_PROJECTS` (default `~/rustd-bench/projects`)
and copied; the clone is never written to. The copy is a git repository, so that a worktree
can be made of it.

Events. Every first-build event is repeated `--runs` times after one warm-up run that is
dropped, `cargo rustd` and plain cargo interleaved, each into a target directory of its own.
The daemon is stopped before every first build, and sccache is off unless the event says.
  cold      no target directory, an empty store
  deps      the dependencies built; the workspace packages cleaned, rustd's records and the
            store removed: load plus compile-all with nothing for rustc to do
  store     the target directory removed, the store kept: load plus the instance walk with
            nothing to compile (rustd only)
  worktree  a git worktree of the copy while the daemon is warm; cargo builds it cold
  sccache   cold with the user's sccache wrapper left on, twice
  edits     no-op, then each edit of the subject made and taken back `--runs` times: body
            (top crate), deep (the crate the others depend on), signature, generic, newfn
            (a function added and called), manifest (a key added to the root Cargo.toml)
After every first build the program is run from its executable: with its version flag five
times (time to `main`, the median), and with its representative arguments twice (the first
and the second run), and its output has to equal the cargo-built program's.

Everything runs under `taskset -c 0-3` (the script pins itself) with the user's `jobs = 4`.
Writes `baseline.json` and `BASELINE.md` next to this file, or where `--out` says.
"""
import datetime, glob, json, os, platform, re, shutil, statistics, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
os.environ.setdefault("RUSTD_PROJECTS", os.path.expanduser("~/rustd-bench/projects"))
os.environ.setdefault("RUSTD_BIN", os.path.expanduser("~/.cargo/bin/rustd"))
import bench, cargo_rustd, genws  # noqa: E402
from cargo_rustd import RUSTD, TOOLCHAIN, Side, fetch, how_built, wait_warm  # noqa: E402

CORES = "0-3"
EVENTS = ("cold", "deps", "store", "worktree", "sccache", "edits")
VERSION_RUNS = 5
# 200 crates compile 50,554 functions in a worker of 9.7 GB, more than this machine has beside
# its other work; 100 is what fits.
CRATES_OF_GENWS = 100
# The large subjects take long enough that fewer runs are what fits a session: genws 200
# compiles 50,554 functions in about nine minutes per cold build.
FEWER_RUNS = {"nushell": 3, "cargo": 3, "genws": 1}


def body(file, opening):
    """An edit that adds a statement after the line that opens a function."""
    return [(file, opening, opening + bench.LET)]


def new_fn(file, opening):
    """An edit that adds a function at the end of `file` and one call to it after `opening`."""
    return [(file, opening, opening + "\n    let _bench_new = _bench_added();"),
            (file, "", "\n#[inline(never)]\nfn _bench_added() -> u32 {\n    7\n}\n")]


def pinned(repo, tag, rev, bin, run, version, edits):
    return dict(repo=repo, tag=tag, rev=rev, bin=bin, run=run, version=version, edits=edits)


SUBJECTS = {
    "hello": dict(make="hello", bin="hello", run=[], version=[],
                  edits=dict(body=body("src/main.rs", "fn main() {"), newfn=new_fn("src/main.rs", "fn main() {"))),
    "ripgrep": pinned("BurntSushi/ripgrep", "15.1.0", "af60c2de9d85e7f3d81c78601669468cf02dabab", "rg",
                      ["-n", "--no-heading", "--sort", "path", "fn main", "crates/core/main.rs", "build.rs"], ["--version"],
                      dict(bench.PROJECTS["ripgrep"]["edits"], newfn=new_fn("crates/core/main.rs", "fn main() -> ExitCode {"))),
    "fd": pinned("sharkdp/fd", "v10.3.0", "d38148f0aabdd073b4080cde770f679f3197b920", "fd",
                 ["--extension", "rs", "main", "src"], ["--version"],
                 dict(body=body("src/main.rs", "fn main() {"), newfn=new_fn("src/main.rs", "fn main() {"))),
    "hyperfine": pinned("sharkdp/hyperfine", "v1.19.0", "12fec42098642a19855ead34c8cb1e0be28c8ead", "hyperfine",
                        ["--help"], ["--version"],
                        dict(body=body("src/main.rs", "fn main() {"), newfn=new_fn("src/main.rs", "fn main() {"))),
    "bat": pinned("sharkdp/bat", "v0.26.0", "c734087e1d15fb730d7653389c841fd9e3016556", "bat",
                  ["--plain", "--color=never", "rustfmt.toml"], ["--version"],
                  dict(body=body("src/bin/bat/main.rs", "fn main() {"), newfn=new_fn("src/bin/bat/main.rs", "fn main() {"))),
    "nushell": pinned("nushell/nushell", "0.108.0", "da141be11b3acaa55b39321e5f9651d02aa2ed6f", "nu",
                      ["-n", "-c", "[3 1 2] | sort | str join ','"], ["--version"],
                      dict(bench.PROJECTS["nushell"]["edits"], newfn=new_fn("src/main.rs", "fn main() -> Result<()> {"))),
    "cargo": pinned("rust-lang/cargo", "0.92.0", "ea2d97820c16195b0ca3fadb4319fe512c199a43", "cargo",
                    ["--version"], ["--version"],
                    dict(bench.PROJECTS["cargo"]["edits"],
                         newfn=new_fn("src/cargo/ops/cargo_compile/mod.rs",
                                      "pub fn compile<'a>(ws: &Workspace<'a>, options: &CompileOptions) -> CargoResult<Compilation<'a>> {"))),
    "genws": dict(make="genws", bin="app", run=["5"], version=["5"], edits={}),
}
for _name, _spec in SUBJECTS.items():
    assert _spec["edits"] is not None and all(_spec.get(k) is not None for k in ("bin", "run", "version")), _name


def pin_cores():
    """Runs this script again under `taskset -c 0-3` when it has more cores than that."""
    try:
        if len(os.sched_getaffinity(0)) <= 4:
            return
    except AttributeError:
        return
    os.execvp("taskset", ["taskset", "-c", CORES, sys.executable, *sys.argv])


def real_cargo():
    """The cargo of the toolchain, never a wrapper that happens to be first on `PATH`."""
    for candidate in (os.environ.get("CARGO"), os.path.join(os.path.dirname(RUSTD), "cargo"), os.path.expanduser("~/.cargo/bin/cargo")):
        if candidate and os.path.exists(candidate):
            return candidate
    return shutil.which("cargo")


def environment(work, name):
    """The environment of one subject: its own daemon home and store, the real cargo first on
    `PATH` and as `CARGO`, sccache off, the daemon's statistics on."""
    env = cargo_rustd.base_env(os.path.join(work, f"home-{name}"))
    cargo = real_cargo()
    env["PATH"] = os.path.dirname(cargo) + os.pathsep + env["PATH"]
    env.update(CARGO=cargo, RUSTD_BACKEND="cranelift", RUSTD_STATS="1", RUSTC_WRAPPER="")
    return env


def wipe(path):
    shutil.rmtree(path, ignore_errors=True)


def apply_edit(ws, changes, back):
    """Makes every change of an edit, or takes it back. An empty `old` appends `new` to the
    file. The result is how many places changed."""
    places = 0
    for pattern, old, new in changes:
        if back:
            old, new = new, old
        for path in sorted(glob.glob(os.path.join(ws, pattern), recursive=True)):
            text = open(path).read()
            if old == "":
                open(path, "w").write(text + new)
                places += 1
            elif new == "":
                assert old in text, (path, old)
                open(path, "w").write(text.replace(old, "", 1))
                places += 1
            elif old in text:
                places += text.count(old)
                open(path, "w").write(text.replace(old, new))
    assert places >= len(changes), f"an edit found only {places} places: {changes}"
    return places


def prepare(name, spec, work, env):
    """The subject's workspace in `work`: a fresh copy of the pinned clone, or a generated one,
    made a git repository with one commit, its registry crates fetched. The result is the
    workspace and its edits."""
    ws = os.path.join(work, name)
    wipe(ws)
    if spec.get("make") == "hello":
        subprocess.run([env["CARGO"], "new", "-q", "--vcs", "none", ws], env=env, check=True)
    elif spec.get("make") == "genws":
        subprocess.run([sys.executable, os.path.join(HERE, "genws.py"), ws, str(CRATES_OF_GENWS)], check=True, capture_output=True)
    else:
        shutil.copytree(fetch(name, spec), ws, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref", ".git"))
    edits = dict(spec["edits"])
    if spec.get("make") == "genws":
        made = genws.edits(ws)
        edits = dict(body=made["body"], deep=made["deep"], signature=made["signature"], generic=made["generic"], newfn=made["new_fn"])
    manifest = open(os.path.join(ws, "Cargo.toml")).read()
    table = "workspace" if "[workspace]" in manifest else "package"
    edits["manifest"] = [("Cargo.toml", "", f"\n[{table}.metadata.rustd_bench]\nedit = 1\n")]
    author = dict(os.environ, GIT_AUTHOR_NAME="bench", GIT_AUTHOR_EMAIL="bench@localhost",
                  GIT_COMMITTER_NAME="bench", GIT_COMMITTER_EMAIL="bench@localhost")
    for args in (["init", "-q"], ["add", "-A"], ["commit", "-qm", "base"]):
        subprocess.run(["git", *args], cwd=ws, env=author, check=True, capture_output=True)
    subprocess.run([env["CARGO"], "fetch"], cwd=ws, env=env, check=True, capture_output=True)
    return os.path.realpath(ws), edits


def members(ws, env):
    """The names of the workspace's packages, for `cargo clean -p`."""
    metadata = subprocess.run([env["CARGO"], "metadata", "--no-deps", "--format-version", "1"], cwd=ws, env=env, capture_output=True, text=True, check=True)
    return [package["name"] for package in json.loads(metadata.stdout)["packages"]]


class Logs:
    """What the daemon's workers wrote since the last look, from every worker and spare log
    under the daemon's home, whichever process wrote it. A new daemon writes to the same
    paths, so the logs are moved aside whenever the daemon is stopped."""

    def __init__(self, home):
        self.home, self.read = home, {}

    def paths(self):
        return [path for kind in ("workers", "spares") for path in glob.glob(os.path.join(self.home, kind, "*", "log"))]

    def rotate(self):
        """Moves every log aside, so what a new daemon writes is read from its start."""
        for path in self.paths():
            os.replace(path, f"{path}.{int(time.time() * 1000)}")
        self.read.clear()

    def said(self, builds=0, each=False):
        """The new text of every log, joined, or one string per log with `each`."""
        texts = []
        for _ in range(100):
            logs = {path: open(path, "rb").read() for path in self.paths()}
            texts = [whole[self.read.get(path, 0):].decode(errors="replace") for path, whole in logs.items()]
            if "".join(texts).count("edit-to-binary:") >= builds:
                break
            time.sleep(0.02)
        self.read.update((path, len(whole)) for path, whole in logs.items())
        return texts if each else "".join(texts)


def first_build_steps(texts):
    """The phases of a first build as the worker that built logged them, in seconds, and what
    the cold compile counted. `texts` is one string per log; the spare that was only started
    for the next build is not the build's time."""
    texts = [texts] if isinstance(texts, str) else texts
    built = [t for t in texts if "cold: compiled" in t]
    text = built[0] if built else "".join(texts)
    steps = bench.cold_steps(text)
    for name, pattern in (("spare", r"^spare: \d+ crates in (\S+)"), ("spare_add", r"^spare: workspace added in (\S+)"),
                          ("entries", r"^entries: (\S+)$")):
        # `entries: 0` is a count the statistics print, not the time listing them took.
        found = [f for f in re.findall(pattern, text, re.M) if bench.DURATION.search(f)]
        if found:
            steps[name] = round(sum(bench.seconds(f) for f in found), 3)
    counted = re.search(r"^cold: compiled (\d+) of (\d+) fns \(\+(\d+) instances\)", text, re.M)
    counts = dict(compiled=int(counted[1]), of=int(counted[2]), instances=int(counted[3])) if counted else {}
    counts["left_to_rustc"] = len(re.findall(r"left to rustc", text))
    return steps, counts


def machine():
    """Load and memory right now, recorded with every sample."""
    load = open("/proc/loadavg").read().split()[0]
    available = int(re.search(r"MemAvailable:\s+(\d+)", open("/proc/meminfo").read())[1]) // 1024
    return dict(load=float(load), mem_available_mb=available)


def cleaned(text):
    text = text.replace("target-ref", "target")
    return re.sub(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z?", "<time>", text)


def sccache_stats(env):
    """sccache's hit and miss counts, when it is there."""
    stats = subprocess.run(["sccache", "--show-stats"], env=env, capture_output=True, text=True)
    hits = re.search(r"Cache hits\s+(\d+)", stats.stdout)
    misses = re.search(r"Cache misses\s+(\d+)", stats.stdout)
    return dict(hits=int(hits[1]) if hits else None, misses=int(misses[1]) if misses else None)


class Measured:
    """One subject's two builds, and what each event measured of them."""

    def __init__(self, name, spec, ws, edits, env, runs):
        self.name, self.spec, self.ws, self.edits, self.env, self.runs = name, spec, ws, edits, env, runs
        self.ours, self.theirs = Side(ws, env, True), Side(ws, env, False)
        self.logs, self.daemon = Logs(env["RUSTD_HOME"]), bench.Daemon(self.ours)
        self.members = members(ws, env)
        self.events, self.wrong, self.daemon_version = {}, [], None

    def stop(self):
        self.ours.cargo("stop")

    def executable(self, side, ws=None):
        target = "target" if side.rustd else "target-ref"
        return os.path.join(ws or self.ws, target, "debug", self.spec["bin"])

    def reset(self, event, side):
        """The state the event asks for, on one side, with the daemon stopped."""
        self.stop()
        self.logs.rotate()
        if event in ("cold", "sccache", "store"):
            wipe(os.path.join(self.ws, "target" if side.rustd else "target-ref"))
        if event == "deps":
            subprocess.run([self.env["CARGO"], "clean", "-q", *(arg for name in self.members for arg in ("-p", name))],
                           cwd=self.ws, env=side.env, capture_output=True)
            if side.rustd:
                wipe(os.path.join(self.ws, "target", "rustd"))
                for records in glob.glob(os.path.join(self.ws, "target", "*", ".rustd-units")):
                    wipe(records)
        if event in ("cold", "sccache", "deps") and side.rustd:
            wipe(self.env["RUSTD_CACHE"])

    def build(self, side, ws=None, env=None):
        """One first build on one side: its wall time, who built it, the worker's phases.
        `env` replaces the side's environment whole, so a variable left out of it is unset."""
        before = machine()
        if ws is not None or env is not None:
            side = Side(ws or self.ws, env or side.env, side.rustd)
        took, ran = side.cargo("build")
        sample = dict(wall_s=round(took, 3), built="cargo", **before)
        if side.rustd:
            sample["built"] = how_built(ran.stderr)
            if ran.returncode == 0 and sample["built"] == "daemon":
                warm, states = wait_warm(side, ws or self.ws)
                sample["warm_s"], sample["states"] = round(warm, 3), states
            said = self.logs.said(each=True)
            sample["steps_s"], sample["counts"] = first_build_steps(said)
            sample["link"] = bench.link_kind("".join(said))
            sample["rss_mb"] = self.daemon.peak_mb()
            if self.daemon_version is None:
                _, status = side.cargo("status")
                version = re.search(r"version (\S+)", status.stdout)
                self.daemon_version = version[1].rstrip(",") if version else None
        if ran.returncode != 0:
            sample["error"] = ran.stderr[-2000:]
            self.wrong.append(f"{self.name}: the {'rustd' if side.rustd else 'cargo'} build failed: {ran.stderr[-300:]!r}")
        return sample

    def run_program(self, side, ws=None):
        """Time to `main` (the median of five runs with the version flag), the representative
        run twice, and what it printed."""
        exe = self.executable(side, ws)
        if not os.path.exists(exe):
            return dict(missing=exe)
        cwd = ws or self.ws
        versions = [cargo_rustd.timed([exe, *self.spec["version"]], cwd, self.env)[0] for _ in range(VERSION_RUNS)]
        first, ran = cargo_rustd.timed([exe, *self.spec["run"]], cwd, self.env)
        second, _ = cargo_rustd.timed([exe, *self.spec["run"]], cwd, self.env)
        return dict(main_ms=round(1000 * statistics.median(versions), 1), run1_s=round(first, 3), run2_s=round(second, 3),
                    output=(ran.returncode, cleaned(ran.stdout), cleaned(ran.stderr)))

    def compare(self, label, mine, theirs):
        """Whether the two programs said the same; the difference is recorded."""
        if "output" not in mine or "output" not in theirs:
            self.wrong.append(f"{self.name} {label}: a program is missing: {mine.get('missing')} {theirs.get('missing')}")
            return False
        if mine["output"] != theirs["output"]:
            self.wrong.append(f"{self.name} {label}: rustd's program says {mine['output']!r}, cargo's {theirs['output']!r}")
            return False
        return True

    def first_builds(self, event):
        """The event's builds, interleaved, one warm-up then `runs` samples per side."""
        samples = []
        sides = [self.ours] if event == "store" else [self.ours, self.theirs]
        env = None
        if event == "sccache":
            env = {k: v for k, v in self.env.items() if k != "RUSTC_WRAPPER"}
            runs = 1
        else:
            runs = self.runs
        for turn in range(runs + 1):
            row = dict(turn=turn, warmup=turn == 0)
            for side in sides:
                name = "rustd" if side.rustd else "cargo"
                self.reset(event, side)
                stats = sccache_stats(env) if event == "sccache" else None
                row[name] = self.build(side, env=env)
                if stats:
                    after = sccache_stats(env)
                    row[name]["sccache"] = {k: (after[k] - stats[k]) if after[k] is not None and stats[k] is not None else None for k in stats}
                row[name]["program"] = self.run_program(side)
                self.stop()
            if event != "store":
                row["same_output"] = self.compare(f"{event} {turn}", row["rustd"]["program"], row["cargo"]["program"])
            samples.append(row)
            self.say(event, row)
        self.events[event] = samples

    def worktrees(self):
        """A worktree of the copy built while the daemon is warm with the copy."""
        samples = []
        self.stop()
        self.logs.rotate()
        warm = self.build(self.ours)
        if warm["built"] != "daemon":
            self.wrong.append(f"{self.name} worktree: the daemon did not build the copy: {warm['built']}")
            return
        # Each turn is a worktree of its own, kept until the end: a path removed and made again
        # is not a new checkout to the daemon, and its worker starts over.
        worktrees = []
        for turn in range(self.runs + 1):
            wt = os.path.realpath(os.path.join(os.path.dirname(self.ws), f"{self.name}-wt{turn}"))
            wipe(wt)
            subprocess.run(["git", "worktree", "add", "-q", wt, "HEAD"], cwd=self.ws, check=True, capture_output=True)
            worktrees.append(wt)
            row = dict(turn=turn, warmup=turn == 0)
            row["rustd"] = self.build(self.ours, ws=wt)
            row["rustd"]["program"] = self.run_program(self.ours, ws=wt)
            row["cargo"] = self.build(self.theirs, ws=wt)
            row["cargo"]["program"] = self.run_program(self.theirs, ws=wt)
            row["same_output"] = self.compare(f"worktree {turn}", row["rustd"]["program"], row["cargo"]["program"])
            samples.append(row)
            self.say("worktree", row)
        for wt in worktrees:
            subprocess.run(["git", "worktree", "remove", "--force", wt], cwd=self.ws, capture_output=True)
            wipe(wt)
        subprocess.run(["git", "worktree", "prune"], cwd=self.ws, capture_output=True)
        self.events["worktree"] = samples

    def edit_phases(self):
        """bench.py's edit loop: no-op, then every edit made and taken back."""
        phases = {}
        for side in (self.ours, self.theirs):
            took, ran = side.cargo("build")
            if ran.returncode != 0:
                self.wrong.append(f"{self.name} edits: the warm build failed on {'rustd' if side.rustd else 'cargo'}")
                return
        wait_warm(self.ours, self.ws)
        self.logs.said()

        def both():
            (took, mine), (reference, other) = self.ours.cargo("build"), self.theirs.cargo("build")
            if mine.returncode != 0 or other.returncode != 0:
                self.wrong.append(f"{self.name} edits: a build failed: {(mine.stderr + other.stderr)[-300:]!r}")
            return took, reference, how_built(mine.stderr)

        noop = [both() for _ in range(self.runs)]
        phases["no-op"] = dict(rustd_s=round(statistics.median(s[0] for s in noop), 3), cargo_s=round(statistics.median(s[1] for s in noop), 3),
                               built=noop[-1][2], samples=[round(s[0], 3) for s in noop])
        self.say("edits", dict(kind="no-op", **phases["no-op"]))
        for kind, changes in self.edits.items():
            made, back = [], []
            for turn in range(self.runs):
                try:
                    places = apply_edit(self.ws, changes, False)
                except AssertionError as why:
                    self.wrong.append(f"{self.name} edits {kind}: {why}")
                    break
                took, reference, built = both()
                said = self.logs.said(1 if built == "daemon" else 0)
                steps, counts = first_build_steps(said)
                made.append(dict(wall_s=took, cargo_s=reference, built=built, link=bench.link_kind(said),
                                 compiled=bench.compiled(said) + counts.get("compiled", 0), reloaded=bool(steps.get("load") or steps.get("spare_add")),
                                 steps_ms=bench.daemon_steps(said), **machine()))
                if turn == 0:
                    self.compare(f"edit {kind}", self.run_program(self.ours), self.run_program(self.theirs))
                    self.logs.said()
                apply_edit(self.ws, changes, True)
                took, reference, built_back = both()
                said = self.logs.said(1 if built_back == "daemon" else 0)
                back.append(dict(wall_s=took, cargo_s=reference, built=built_back, link=bench.link_kind(said)))
                if built != "daemon" or built_back != "daemon":
                    break
            if not made:
                continue
            median = lambda rows, key: round(statistics.median(r[key] for r in rows), 3)
            slowest = max(made, key=lambda s: s["wall_s"])
            phases[kind] = dict(rustd_s=median(made, "wall_s"), cargo_s=median(made, "cargo_s"), built=slowest["built"], link=slowest["link"],
                                reloaded=any(s["reloaded"] for s in made), compiled=max(s["compiled"] for s in made), places=places,
                                slowest_s=round(slowest["wall_s"], 3), samples=[round(s["wall_s"], 3) for s in made],
                                back_rustd_s=median(back, "wall_s"), back_cargo_s=median(back, "cargo_s"), steps_ms=slowest["steps_ms"])
            self.say("edits", dict(kind=kind, **phases[kind]))
        self.events["edits"] = phases

    def say(self, event, row):
        if event == "edits":
            print(f"  {event:9} {row['kind']:10} rustd {row['rustd_s']:8.3f}s  cargo {row['cargo_s']:8.3f}s  {row['built'][:50]}  {row.get('link') or ''}", flush=True)
            return
        mine, theirs = row.get("rustd", {}), row.get("cargo", {})
        steps = mine.get("steps_s", {})
        print(f"  {event:9} run {row['turn']}{'w' if row['warmup'] else ' '} rustd {mine.get('wall_s', 0):8.3f}s  cargo {theirs.get('wall_s', 0):8.3f}s  "
              f"spare {steps.get('spare', 0):5.1f} load {steps.get('load', 0):5.1f} compile {steps.get('compile', 0):5.1f} link {steps.get('link', 0):5.2f}  "
              f"fns {mine.get('counts', {}).get('compiled', '?')}  rss {mine.get('rss_mb', '?')} MB  main {mine.get('program', {}).get('main_ms', '?')} ms  "
              f"{mine.get('built', '')[:40]}  {'same' if row.get('same_output', True) else 'DIFFERENT'}", flush=True)


def context(runs, events):
    """What every number of a run was measured on, recorded once per result file."""
    cpu = re.search(r"model name\s*:\s*(.*)", open("/proc/cpuinfo").read())
    memory = re.search(r"MemTotal:\s+(\d+)", open("/proc/meminfo").read())
    harness = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=HERE, capture_output=True, text=True).stdout.strip()
    config = os.path.expanduser("~/.cargo/config.toml")
    return dict(date=datetime.datetime.now().astimezone().isoformat(timespec="seconds"), toolchain=TOOLCHAIN, rustd_bin=RUSTD,
                harness_commit=harness, cpu=cpu[1] if cpu else platform.processor(), cores=CORES, logical_cpus=os.cpu_count(),
                memory_gb=round(int(memory[1]) / 2**20, 1) if memory else None, kernel=platform.release(), python=platform.python_version(),
                cargo=real_cargo(), cargo_config=open(config).read() if os.path.exists(config) else None,
                backend="cranelift", sccache_off_except="sccache event", runs=runs, fewer_runs=FEWER_RUNS, events=events,
                env=dict(RUSTD_STATS="1", RUSTC_WRAPPER="", RUSTD_BACKEND="cranelift"))


def medians(samples, side, key, inner=None):
    """The median and the spread of one number over the samples that are not the warm-up."""
    values = []
    for row in samples:
        if row.get("warmup") or side not in row:
            continue
        value = row[side].get(inner, {}).get(key) if inner else row[side].get(key)
        if isinstance(value, (int, float)):
            values.append(value)
    if not values:
        return None
    return dict(median=round(statistics.median(values), 3), min=round(min(values), 3), max=round(max(values), 3), n=len(values))


def fmt(stat, unit="s", digits=2):
    if not stat:
        return ""
    return f"{stat['median']:.{digits}f} ({stat['min']:.{digits}f} to {stat['max']:.{digits}f})"


def table(result):
    """BASELINE.md: one table per event, subjects as rows."""
    out = ["# Baseline: the first build through `cargo rustd` against plain cargo", ""]
    c = result["context"]
    out += [f"Measured {c['date']} on {c['cpu']}, cores {c['cores']} of {c['logical_cpus']}, {c['memory_gb']} GB, kernel {c['kernel']}, "
            f"Rust {c['toolchain']}, rustd `{c['rustd_bin']}` (daemon version {result.get('daemon_version')}), harness {c['harness_commit']}, "
            f"Cranelift, sccache off except in its own event, `jobs = 4` and the linker from the user's cargo config. "
            f"Seconds, the median over the runs after a warm-up, with the minimum and maximum in brackets. Phases are the worker's own timings.", ""]
    for event in ("cold", "deps", "store", "worktree", "sccache"):
        rows = [(name, r["events"][event]) for name, r in result["results"].items() if event in r["events"]]
        if not rows:
            continue
        out += [f"## {event}", "", "| Subject | runs | cargo build | rustd build | spare | load | resolve | entries | compile | emit | link | functions (+instances) | daemon RSS MB | to main cargo / rustd ms | run 1 / run 2 rustd s | run 1 cargo s | output |",
                "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|"]
        for name, samples in rows:
            steps = lambda key: fmt(medians(samples, "rustd", key, "steps_s"))
            counts = [row["rustd"].get("counts", {}) for row in samples if "rustd" in row]
            fns = max((c.get("compiled", 0) for c in counts), default=0)
            inst = max((c.get("instances", 0) for c in counts), default=0)
            same = all(row.get("same_output", True) for row in samples)
            rss = max((row["rustd"].get("rss_mb", 0) for row in samples if "rustd" in row), default=0)
            main_c, main_r = medians(samples, "cargo", "main_ms", "program"), medians(samples, "rustd", "main_ms", "program")
            out.append(f"| {name} | {medians(samples, 'rustd', 'wall_s')['n'] if medians(samples, 'rustd', 'wall_s') else 0} | {fmt(medians(samples, 'cargo', 'wall_s'))} | {fmt(medians(samples, 'rustd', 'wall_s'))} | "
                       f"{steps('spare')} | {steps('load')} | {steps('resolve')} | {steps('entries')} | {steps('compile')} | {steps('emit')} | {steps('link')} | "
                       f"{fns} (+{inst}) | {rss} | {main_c['median'] if main_c else ''} / {main_r['median'] if main_r else ''} | "
                       f"{fmt(medians(samples, 'rustd', 'run1_s', 'program'))} / {fmt(medians(samples, 'rustd', 'run2_s', 'program'))} | {fmt(medians(samples, 'cargo', 'run1_s', 'program'))} | "
                       f"{'same' if same else 'DIFFERENT'} |")
        out.append("")
    rows = [(name, r["events"]["edits"]) for name, r in result["results"].items() if "edits" in r["events"]]
    if rows:
        out += ["## edits", "", "| Subject | edit | cargo s | rustd s | rustd samples | link | reloaded | functions compiled | revert cargo / rustd s |", "|---|---|---:|---:|---|---|---|---:|---:|"]
        for name, phases in rows:
            for kind, p in phases.items():
                out.append(f"| {name} | {kind} | {p['cargo_s']:.3f} | {p['rustd_s']:.3f} | {p.get('samples')} | {p.get('link') or ''} | {p.get('reloaded', '')} | {p.get('compiled', '')} | "
                           f"{p.get('back_cargo_s', '')} / {p.get('back_rustd_s', '')} |")
        out.append("")
    wrong = [f"- {w}" for r in result["results"].values() for w in r["wrong"]]
    out += ["## Differences and failures", ""] + (wrong or ["none"]) + [""]
    return "\n".join(out)


def main():
    pin_cores()
    argv = sys.argv[1:]
    runs = int(bench.options(argv, "--runs", "5")[0])
    events = bench.options(argv, "--events", ",".join(EVENTS))[0].split(",")
    work = os.path.expanduser((bench.options(argv, "--work") or [None])[0] or "~/rustd-bench/work")
    out = bench.options(argv, "--out", os.path.join(HERE, "baseline.json"))[0]
    keep = "--keep" in argv
    names = [a for a in argv if not a.startswith("--")] or list(SUBJECTS)
    unknown = [e for e in events if e not in EVENTS] + [n for n in names if n not in SUBJECTS]
    if unknown:
        raise SystemExit(f"unknown: {unknown}; events are {EVENTS}, subjects {list(SUBJECTS)}")
    os.makedirs(work, exist_ok=True)
    result = dict(context=context(runs, events), results={})
    if os.path.exists(out):
        result = json.load(open(out))
        result["context"] = context(runs, events)
    for name in names:
        spec = SUBJECTS[name]
        subject_runs = min(runs, FEWER_RUNS.get(name, runs))
        print(f"{name}: {subject_runs} runs, events {events}, work {work}", flush=True)
        env = environment(work, name)
        try:
            ws, edits = prepare(name, spec, work, env)
            measured = Measured(name, spec, ws, edits, env, subject_runs)
            for event in events:
                if event == "worktree":
                    measured.worktrees()
                elif event == "edits":
                    measured.edit_phases()
                else:
                    measured.first_builds(event)
            measured.stop()
            # A run of some events adds to what an earlier run of the others measured.
            earlier = result["results"].get(name, {})
            result["results"][name] = dict(events={**earlier.get("events", {}), **measured.events},
                                           wrong=[w for w in earlier.get("wrong", []) if not any(w.startswith(f"{name} {e}") for e in events)] + measured.wrong,
                                           runs=subject_runs, tag=spec.get("tag"), members=len(measured.members))
            result["daemon_version"] = measured.daemon_version or result.get("daemon_version")
        except Exception as why:  # one subject failing must not lose the others
            print(f"  FAILED {name}: {why!r}", flush=True)
            earlier = result["results"].get(name, {})
            result["results"][name] = dict(events=earlier.get("events", {}), wrong=earlier.get("wrong", []) + [f"{name}: {why!r}"],
                                           runs=subject_runs, tag=spec.get("tag"))
        for line in result["results"][name]["wrong"]:
            print(f"  WRONG {line[:300]}", flush=True)
        json.dump(result, open(out, "w"), indent=1)
        open(os.path.splitext(out)[0].replace("baseline", "BASELINE") + ".md", "w").write(table(result))
        if not keep:
            for entry in os.listdir(work):
                if entry in (name, f"home-{name}") or entry.startswith(f"{name}-wt"):
                    wipe(os.path.join(work, entry))
    wrong = [w for r in result["results"].values() for w in r["wrong"]]
    print(f"wrote {out}; differences and failures: {len(wrong)}")
    sys.exit(1 if wrong else 0)


if __name__ == "__main__":
    main()
