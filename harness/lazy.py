#!/usr/bin/env python3
"""lazy.py: a lazy build (`--lazy`) compiles the eager set and puts a stub in for every other
root the program reaches.

usage: lazy.py

One package whose `main` runs the case named by its first argument, each case a way for code
to be called by something other than compiled code of ours: an exported function, C callbacks
(`atexit`, `signal`), an `.init_array` constructor, a function pointer in a `#[used]` static,
inline and naked assembly. Built by the worker one-shot, as difftest.py does, with no rustc
copy of the package to hide a mistake behind (`RUSTD_ALONE`):

- lazily, every case but `stub` prints what cargo's build does; `stub` calls a plain function,
  which is a stub, and dies with SIGILL after what it printed before the call;
- the report has an `eager` row with the expected reason for each eager function, a
  `stubbed` row for each stub and no `skipped` row, and `stubbed N fns` agrees with it;
- the object has one 16-byte stub with its own index and one frame entry of range 16 per
  stubbed row, and a `.rustd.lazy` table kept from `--gc-sections` whose rows, read from the
  linked binary, each point at their stub;
- eagerly, every case prints what cargo's build does and there is no table.

Then the difftest corpus program built lazily: no stub's declaration disagrees with its
callers' (no `skipped` row), and what is compiled is a part of what an eager build compiles.
Linux only: the table is written for ELF.
"""
import os, re, struct, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from difftest import Fixture, RUSTD, label  # noqa: E402

SOURCE = r'''
use std::arch::{asm, naked_asm};

// Exported: a runtime looks it up by name. Called through a pointer only.
#[no_mangle]
pub extern "C" fn exported_fn() -> u32 {
    println!("exported_fn");
    11
}

// Foreign ABI: a C library calls them.
extern "C" fn bye() {
    println!("bye");
}

extern "C" fn on_signal(_: i32) {
    println!("on_signal");
}

unsafe extern "C" {
    fn atexit(f: extern "C" fn()) -> i32;
    fn signal(signal: i32, handler: extern "C" fn(i32)) -> usize;
    fn raise(signal: i32) -> i32;
}

// Run by the loader before main, from .init_array.
extern "C" fn init() {
    println!("init");
}

#[used]
#[link_section = ".init_array"]
static INIT: extern "C" fn() = init;

// A plain function in a #[used] static, called through the static.
fn register() -> u32 {
    println!("register");
    22
}

#[used]
static REG: fn() -> u32 = register;

// Inline assembly, called directly.
fn with_asm() -> u64 {
    let x: u64;
    #[cfg(target_arch = "x86_64")]
    unsafe { asm!("mov {0}, 42", out(reg) x) };
    #[cfg(target_arch = "aarch64")]
    unsafe { asm!("mov {0}, #42", out(reg) x) };
    x
}

// Naked, and so foreign ABI too.
#[unsafe(naked)]
extern "C" fn naked_answer() -> u64 {
    #[cfg(target_arch = "x86_64")]
    naked_asm!("mov eax, 7", "ret");
    #[cfg(target_arch = "aarch64")]
    naked_asm!("mov x0, #7", "ret");
}

// Inline assembly reached by nothing: only the rule compiles it.
fn unreached_asm() -> u64 {
    let x: u64;
    unsafe { asm!("nop", "mov {0}, {0}", inout(reg) 3u64 => x) };
    x
}

#[allow(dead_code)]
fn unreached_root() -> fn() -> u64 {
    unreached_asm
}

// A plain function: the program's stub.
fn lazy_target() -> u32 {
    println!("lazy_target");
    44
}

fn main() {
    let case = std::env::args().nth(1).unwrap_or_default();
    println!("case {case}");
    match case.as_str() {
        "hello" => println!("hello"),
        "exported" => {
            let f: extern "C" fn() -> u32 = exported_fn;
            println!("{}", f());
        }
        "callback" => unsafe {
            atexit(bye);
            signal(10, on_signal);
            raise(10);
        },
        "ctor" => println!("after init"),
        "table" => {
            let r = unsafe { std::ptr::read_volatile(&raw const REG) };
            println!("{}", r());
        }
        "asm" => println!("{} {}", with_asm(), naked_answer()),
        "stub" => println!("{}", lazy_target()),
        _ => println!("no such case"),
    }
}
'''

CASES = ["hello", "exported", "callback", "ctor", "table", "asm", "stub"]
# Each eager function's first reason, in the order of `EagerReason`. `init` and `on_signal`
# are foreign ABI before anything else; `naked_answer` is foreign ABI before asm.
REASONS = {"main": "main", "exported_fn": "exported", "bye": "foreign_abi", "on_signal": "foreign_abi",
           "init": "foreign_abi", "naked_answer": "foreign_abi", "with_asm": "asm", "unreached_asm": "asm",
           "register": "pointed_from_kept_static"}
STUBBED = ["lazy_target"]
SIGILL = -4


def sh(*args):
    return subprocess.run(args, capture_output=True, text=True, check=True).stdout


def short(name):
    return name.rsplit("::", 1)[-1]


def arch():
    return subprocess.check_output(["rustc", "-vV"], text=True).split("host: ")[1].split("-")[0]


def stub_bytes(index):
    """What `Arch::stub` writes for row `index`."""
    if arch() == "aarch64":
        words = [0x52800010 | ((index & 0xFFFF) << 5), 0x72A00010 | ((index >> 16) << 5), 0, 0xD503201F]
        return b"".join(struct.pack("<I", w) for w in words)
    return b"\xb8" + struct.pack("<I", index) + b"\x0f\x0b" + b"\xcc" * 9


def sections(path):
    """name -> (address, file offset, size, sh_flags) of each section."""
    found, lines = {}, sh("readelf", "-SW", "-t", path).splitlines()
    for at, line in enumerate(lines):
        m = re.match(r"\s*\[\s*\d+\]\s+(\S+)$", line)
        if not m or at + 2 >= len(lines):
            continue
        fields = lines[at + 1].split()
        flags = re.search(r"\[([0-9a-f]+)\]", lines[at + 2])
        found[m[1]] = (int(fields[1], 16), int(fields[2], 16), int(fields[3], 16), int(flags[1], 16) if flags else 0)
    return found


def symbols(path):
    """name -> (address, size, type, visibility) of each defined symbol."""
    found = {}
    for line in sh("readelf", "-sW", path).splitlines():
        p = line.split()
        if len(p) >= 8 and p[6] != "UND" and re.match(r"\d+:$", p[0]):
            found[p[7]] = (int(p[1], 16), int(p[2], 0), p[3], p[5])
    return found


def bytes_at(path, address, length):
    data = open(path, "rb").read()
    for addr, off, size, _ in sections(path).values():
        if addr and addr <= address < addr + size:
            return data[off + address - addr: off + address - addr + length]
    raise AssertionError(f"{address:#x} is in no section of {path}")


def check(ok, what):
    if not ok:
        raise SystemExit(f"FAIL: {what}")


def check_report(fx, crate, stderr):
    """The report's rows and status lines. Returns the stubbed names, in index order."""
    rows = fx.reports[crate][1:]
    of = lambda kind: [r[1:] for r in rows if r[0] == kind]
    compiled, stubbed, eager = [r[0] for r in of("compiled")], [r[0] for r in of("stubbed")], of("eager")
    check(not of("skipped"), f"skipped rows: {of('skipped')}")
    line = re.search(r"^stubbed (\d+) fns$", stderr, re.M)
    check(line and int(line[1]) == len(stubbed), f"`stubbed N fns` is {line and line[0]}, rows {len(stubbed)}")
    check(not set(stubbed) & set(compiled), "a function both compiled and stubbed")
    check({r[0] for r in eager} <= set(compiled), "an eager function that is not compiled")
    return compiled, stubbed, {short(r[0]): r[1] for r in eager}


def check_object(obj, binary, stubbed):
    """The stubs and the table, in the object and in the linked binary."""
    syms = symbols(obj)
    table = sections(obj).get(".rustd.lazy")
    check(table, "no .rustd.lazy section in the object")
    check(table[3] & 0x2 and not table[3] & 0x1 and table[3] & 0x200000,
          f".rustd.lazy flags {table[3]:#x}: want ALLOC and SHF_GNU_RETAIN, not WRITE")
    check(syms.get("rustd_lazy_table", (0, 0, "", ""))[2:] == ("OBJECT", "HIDDEN"), "rustd_lazy_table is no hidden object")
    relocs = sh("readelf", "-rW", obj).split("'.rela.rustd.lazy'")
    kinds = re.findall(r"^\s*[0-9a-f]+\s+[0-9a-f]+\s+(R_\w+)", relocs[1].split("Relocation section")[0], re.M) if len(relocs) > 1 else []
    check(len(kinds) == len(stubbed) and set(kinds) <= {"R_X86_64_PC32", "R_AARCH64_PREL32"}, f"relocations into the table: {kinds}")
    frames = sh("readelf", "--debug-dump=frames", binary)
    funcs = {}
    for name, (address, size, kind, _) in symbols(binary).items():
        if kind == "FUNC":
            funcs.setdefault(address, []).append((name, size))
    address, size = symbols(binary)["rustd_lazy_table"][:2]
    raw = bytes_at(binary, address, size)
    magic, version, count = struct.unpack_from("<8sII", raw, 0)
    check((magic, version) == (b"rustdlzy", 1), f"table header {magic!r} {version}")
    check(count == len(stubbed), f"the table has {count} rows, the report {len(stubbed)} stubs")
    rows_off, pool_off = struct.unpack_from("<II", raw, 88)
    rustd_off, rustd_len = struct.unpack_from("<II", raw, 72)
    check(os.path.realpath(raw[pool_off + rustd_off:][:rustd_len]) == os.path.realpath(RUSTD).encode(), "the table names another rustd")
    for index in range(count):
        at = rows_off + index * 16
        target = address + at + struct.unpack_from("<i", raw, at)[0]
        here = funcs.get(target, [])
        check(any(size == 16 for _, size in here), f"row {index} points at {target:#x}, which is {here}")
        check(bytes_at(binary, target, 16) == stub_bytes(index), f"row {index}'s stub {here} does not hold its index")
        check(len(re.findall(r"pc=%016x\.\.%016x" % (target, target + 16), frames)) == 1, f"no frame entry of range 16 for {here}")
        # The stable name: of a workspace crate it is by content, `fn ~<hash>:<file>#...`.
        name_off, name_len = struct.unpack_from("<II", raw, at + 4)
        check(raw[pool_off + name_off:][:name_len].startswith(b"fn "), f"row {index} ({stubbed[index]}) has no function's name")
    return count


def fixture():
    ws = tempfile.mkdtemp(prefix="rustd-lazy-")
    os.makedirs(f"{ws}/src")
    open(f"{ws}/Cargo.toml", "w").write('[package]\nname = "lazy"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n')
    open(f"{ws}/src/main.rs", "w").write(SOURCE)
    built = subprocess.run(["cargo", "build", "-p", "lazy"], cwd=ws, capture_output=True, text=True)
    check(built.returncode == 0, f"cargo build: {built.stderr[-3000:]}")
    fx = Fixture(ws, ["lazy"], ["-p", "lazy"])
    fx.reference = f"{ws}/target/debug/lazy"
    expected = {}
    for case in CASES:
        fx.run_args = [case]
        expected[case] = fx.behaviour(fx.reference)
        check(expected[case][0] == 0, f"cargo's build of case {case}: {expected[case]}")
    # No rustc copy of the package: a function rustd turned down would make the build unfit.
    fx.alone = True
    fx.rustc_build()
    return fx, expected


def lazy_fixture(fx, expected):
    fx.rustd_build(isolate=False, extra_args=["--lazy"])
    stderr = open(os.path.join(fx.work, "lazy.stderr")).read()
    compiled, stubbed, reasons = check_report(fx, "lazy", stderr)
    print(f"lazy: {len(compiled)} compiled, {len(stubbed)} stubbed: {[short(s) for s in stubbed]}")
    check(reasons == REASONS, f"eager reasons {reasons}, want {REASONS}")
    check([short(s) for s in stubbed] == STUBBED, f"stubbed {stubbed}, want {STUBBED}")
    check_object(fx.objects[0], fx.binary, stubbed)
    print("lazy: the table's rows point at their stubs; one frame entry each")
    for case in CASES:
        fx.run_args = [case]
        got = fx.behaviour(fx.binary)
        if case == "stub":
            want = expected[case][1].split(b"lazy_target\n")[0]
            check(got[0] == SIGILL and got[1] == want, f"case stub: {got}, want SIGILL after {want!r}")
        else:
            check(got == expected[case], f"case {case}: {got}, cargo's {expected[case]}")
    print(f"lazy: {len(CASES) - 1} cases print what cargo's build does; `stub` dies with SIGILL in the stub")


def eager_fixture(fx, expected):
    fx.rustd_build(isolate=False)
    rows = fx.reports["lazy"]
    check(not [r for r in rows if r[0] in ("stubbed", "eager", "skipped")], "an eager build with stubbed, eager or skipped rows")
    check(".rustd.lazy" not in sections(fx.objects[0]), "an eager object with a table")
    for case in CASES:
        fx.run_args = [case]
        got = fx.behaviour(fx.binary)
        check(got == expected[case], f"eager case {case}: {got}, cargo's {expected[case]}")
    print(f"eager: {len(CASES)} cases print what cargo's build does; no table")


def signatures():
    """Every root of the corpus program that is reached: no stub's declaration is refused."""
    crate = "runner,cases"
    fx = Fixture(os.path.join(HERE, "corpus"), [crate], ["-p", "runner"])
    fx.rustc_build()
    fx.rustd_build(isolate=False)
    eager = {r[1] for r in fx.reports[crate] if r[0] == "compiled"}
    fx.rustd_build(isolate=False, extra_args=["--lazy"])
    stderr = open(os.path.join(fx.work, f"{label(crate)}.stderr")).read()
    compiled, stubbed, _ = check_report(fx, crate, stderr)
    check(set(compiled) <= eager, "the lazy corpus build compiles what the eager one does not")
    check(stubbed, "the lazy corpus build has no stubs")
    count = check_object(fx.objects[0], fx.binary, stubbed)
    print(f"corpus: {len(compiled)} of {len(eager)} roots compiled, {count} stubs, no signature refused")


def main():
    if not sys.platform.startswith("linux"):
        print("lazy.py: lazy builds are ELF only; skipped")
        return
    fx, expected = fixture()
    lazy_fixture(fx, expected)
    eager_fixture(fx, expected)
    signatures()
    print("lazy: OK")


if __name__ == "__main__":
    main()
