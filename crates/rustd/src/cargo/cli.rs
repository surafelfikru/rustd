//! `cargo rustd <subcommand> [flags] [-- args]`: the flags are cargo's and go to cargo as they
//! are. The few read here decide whether rustd has a part in the build and which build of
//! the checkout it is.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use sha2::{Digest as _, Sha256};

use crate::plan::Selection;

/// The flags of cargo's build commands that take their value as the next argument.
const VALUED: &[&str] = &[
    "-p",
    "--package",
    "--exclude",
    "-j",
    "--jobs",
    "--bin",
    "--example",
    "--test",
    "--bench",
    "-F",
    "--features",
    "--profile",
    "--target",
    "--target-dir",
    "--artifact-dir",
    "--manifest-path",
    "--lockfile-path",
    "--message-format",
    "--color",
    "--config",
    "-Z",
    "-C",
    "--eager",
];

/// The flags that pick targets of a package, after which cargo runs no doc tests.
const TARGET_SELECTION: &[&str] = &[
    "--lib",
    "--bins",
    "--bin",
    "--examples",
    "--example",
    "--tests",
    "--test",
    "--benches",
    "--bench",
    "--all-targets",
];

/// The flags of `cargo test` that are about running and not about building.
const RUNNING: &[&str] = &["--no-run", "--no-fail-fast", "--rerun-all"];

/// The flags of `cargo rustd` alone, which cargo is never given.
const OURS: &[&str] = &["--eager"];

/// The variables that change what cargo builds or what the daemon compiles with, by name
/// or by what the name starts with. Their values are part of which build this is.
const BUILD_ENV: &[&str] = &[
    "RUSTFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTUP_TOOLCHAIN",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_",
    "CARGO_PROFILE_",
    "CARGO_INCREMENTAL",
    "RUSTD_BACKEND",
    "RUSTD_OPT",
    "RUSTD_EMIT",
    "RUSTD_SPLICE",
];

/// The variables a worker is started with: the ones that pick its toolchain and its backend,
/// and the ones cargo reads flags and profiles from.
const WORKER_ENV: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_TARGET_DIR",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_",
    "CARGO_PROFILE_",
    "RUSTUP_TOOLCHAIN",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "PATH",
    "RUSTD_BACKEND",
    "RUSTD_OPT",
    "RUSTD_EMIT",
    "RUSTD_SPLICE",
];

/// Whether a build compiles every function it reaches, or only the ones the program needs
/// before it runs, with a stub that aborts for each of the rest. The lazy build is for
/// measuring what compiling on first call would save; it is not a program to ship.
///
/// `cargo rustd` reads it from `--eager on|off`, or else from `RUSTD_LAZY` (`1` for lazy,
/// `0` or unset for eager), and tells the worker with `--lazy`. It is part of which build
/// this is, so a lazy build and an eager one of the same checkout have workers of their own.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum Compilation {
    Eager,
    Lazy,
}

/// The variable a lazy build is asked for by, when `--eager` is not given.
const LAZY_ENV: &str = "RUSTD_LAZY";

impl Compilation {
    /// From the value of `--eager`.
    fn of_flag(value: Option<&str>) -> Result<Compilation, String> {
        match value {
            Some("on") => Ok(Compilation::Eager),
            Some("off") => Ok(Compilation::Lazy),
            Some(other) => Err(format!("`--eager {other}`: the value is `on` or `off`")),
            None => Err("`--eager` takes `on` or `off`".to_owned()),
        }
    }

    /// From the value of `RUSTD_LAZY`, unset or empty being eager.
    fn of_env(value: Option<&str>) -> Result<Compilation, String> {
        match value {
            None | Some("" | "0") => Ok(Compilation::Eager),
            Some("1") => Ok(Compilation::Lazy),
            Some(other) => Err(format!("`{LAZY_ENV}={other}`: the value is `1` or `0`")),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Subcommand {
    Build,
    Run,
    Test,
    /// Clippy's lints, which the daemon finds once it has built the programs.
    Clippy,
}

impl Subcommand {
    fn named(name: &str) -> Option<Subcommand> {
        match name {
            "build" | "b" => Some(Subcommand::Build),
            "run" | "r" => Some(Subcommand::Run),
            "test" | "t" => Some(Subcommand::Test),
            "clippy" => Some(Subcommand::Clippy),
            _ => None,
        }
    }
}

/// What the user asked for.
#[derive(Debug, PartialEq)]
pub(crate) enum Asked {
    Build(Box<Invocation>),
    /// A subcommand that is cargo's alone, with its arguments.
    Cargo(Vec<String>),
    Status,
    Stop,
    Enable,
    Disable,
    Help,
}

/// A build command and its arguments, split into what is cargo's to read and what is the
/// program's.
#[derive(Debug, PartialEq)]
pub(crate) struct Invocation {
    pub(crate) subcommand: Subcommand,
    /// Everything after the subcommand, as typed.
    pub(crate) typed: Vec<String>,
    /// The flags that say what to build.
    pub(crate) build_args: Vec<String>,
    /// The words before `--` that are no flags: arguments of the program for `run`, the
    /// names of the tests to run for `test`.
    pub(crate) positional: Vec<String>,
    /// What follows `--`, for the program or the test executables.
    pub(crate) trailing: Vec<String>,
    pub(crate) flags: Flags,
}

/// The flags rustd reads.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Flags {
    pub(crate) manifest_path: Option<PathBuf>,
    pub(crate) packages: Vec<String>,
    pub(crate) workspace: bool,
    pub(crate) excludes: Vec<String>,
    pub(crate) features: Vec<String>,
    pub(crate) all_features: bool,
    pub(crate) no_default_features: bool,
    pub(crate) profile: Option<String>,
    pub(crate) targets: Vec<String>,
    pub(crate) bin: Option<String>,
    pub(crate) example: Option<String>,
    /// The shapes `--message-format` asks cargo's messages in, each of them.
    pub(crate) message_format: Vec<String>,
    /// `--build-plan` or `--unit-graph`, which ask cargo for its plan instead of a build.
    pub(crate) plans: Option<String>,
    pub(crate) unstable: bool,
    pub(crate) quiet: bool,
    pub(crate) no_run: bool,
    pub(crate) no_fail_fast: bool,
    /// Whether every test is to run, the ones whose result is known too.
    pub(crate) rerun_all: bool,
    pub(crate) doc: bool,
    /// The flag that has tests or benchmarks built, when one does.
    pub(crate) tests: Option<String>,
    /// Whether a flag picks targets, so that `cargo test` runs no doc tests.
    pub(crate) selects_targets: bool,
    /// What `--eager` asked for, or why its value is not one.
    pub(crate) compilation: Option<Result<Compilation, String>>,
}

impl Asked {
    /// From the arguments after `cargo rustd`.
    pub(crate) fn parse(args: Vec<String>) -> Asked {
        let Some((first, rest)) = args.split_first() else {
            return Asked::Help;
        };
        match (first.as_str(), Subcommand::named(first)) {
            ("--help" | "-h" | "help", _) => Asked::Help,
            (_, Some(subcommand)) => Asked::Build(Box::new(Invocation::parse(subcommand, rest))),
            ("status", _) => Asked::Status,
            ("stop", _) => Asked::Stop,
            ("enable", _) => Asked::Enable,
            ("disable", _) => Asked::Disable,
            _ => Asked::Cargo(args),
        }
    }
}

impl Invocation {
    fn parse(subcommand: Subcommand, typed: &[String]) -> Invocation {
        let (before, after) = match typed.iter().position(|it| it == "--") {
            Some(at) => (&typed[..at], &typed[at + 1..]),
            None => (typed, &[][..]),
        };
        let mut flags = Flags::default();
        let mut build_args = Vec::new();
        let mut positional = Vec::new();
        let mut words = before.iter();
        while let Some(word) = words.next() {
            if !word.starts_with('-') {
                positional.push(word.clone());
                continue;
            }
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
                // `-pname`, `-j8`.
                _ if !word.starts_with("--") && word.len() > 2 && VALUED.contains(&&word[..2]) => {
                    (&word[..2], Some(word[2..].to_owned()))
                }
                _ => (word.as_str(), None),
            };
            let value = match joined {
                None if VALUED.contains(&flag) => words.next().cloned(),
                joined => joined,
            };
            flags.read(flag, value.as_deref());
            if RUNNING.contains(&flag) || OURS.contains(&flag) {
                continue;
            }
            build_args.push(flag.to_owned());
            build_args.extend(value);
        }
        let trailing = after.to_vec();
        // Cargo has no such flags, and is given what was typed when the build is its own.
        let mut typed_ours = vec![false; typed.len()];
        for (at, word) in before.iter().enumerate() {
            let flag = word.split_once('=').map_or(word.as_str(), |it| it.0);
            if word == "--rerun-all" || OURS.contains(&flag) {
                typed_ours[at] = true;
                // `--eager off`: the value is the next word.
                if !word.contains('=') && VALUED.contains(&flag) && at + 1 < before.len() {
                    typed_ours[at + 1] = true;
                }
            }
        }
        let typed = typed.iter().zip(typed_ours).filter(|it| !it.1).map(|it| it.0.clone());
        let typed = typed.collect();
        Invocation { subcommand, typed, build_args, positional, trailing, flags }
    }

    /// What the program or a test executable is run with: `cargo run a -- b` runs the
    /// program with `a b`, and `cargo test a -- b` the tests named like `a`, with `b`.
    pub(crate) fn program_args(&self) -> impl Iterator<Item = &String> {
        self.positional.iter().chain(&self.trailing)
    }

    /// Which build of the checkout this is: a hash of what cargo is asked to build, where,
    /// and with what in the environment.
    pub(crate) fn key(&self, cwd: &Path) -> String {
        let mut hash = Sha256::new();
        // `run` builds what `build` does.
        let builds = if self.subcommand == Subcommand::Test { "test" } else { "build" };
        hash.update(builds);
        // Not in the arguments: `--eager off` and `RUSTD_LAZY=1` are the same build. An eager
        // build hashes nothing for it, so its key is what it was before lazy builds existed.
        if self.compilation() == Ok(Compilation::Lazy) {
            hash.update([3]);
            hash.update("lazy");
        }
        hash.update(cwd.as_os_str().as_encoded_bytes());
        for arg in &self.build_args {
            hash.update([0]);
            hash.update(arg);
        }
        for (name, value) in build_env() {
            hash.update([1]);
            hash.update(name);
            hash.update([2]);
            hash.update(value);
        }
        hash.finalize().iter().take(8).map(|it| format!("{it:02x}")).collect()
    }

    /// Why rustd does not build this, when it is clear before anything is built. Nothing
    /// else builds it then: the command fails, with this.
    pub(crate) fn unbuilt(&self) -> Option<String> {
        let surroundings = match () {
            _ if another_wrapper() => {
                "RUSTC_WORKSPACE_WRAPPER is set to another wrapper, which `cargo rustd` \
                 does not run rustc through"
            }
            _ => return missing_wrapper().or_else(|| self.unbuilt_flag()),
        };
        Some(surroundings.to_owned())
    }

    /// Why what the command builds of the tests of the workspace is not built, when it builds
    /// something that is not.
    pub(crate) fn builds_tests(&self) -> Option<String> {
        let flag = self.flags.tests.as_deref();
        // The daemon writes the harness of unit and integration tests; benchmarks are not built.
        match flag {
            Some(flag @ ("--benches" | "--bench" | "--all-targets")) => Some(format!(
                "`{flag}`: the daemon does not build the benchmarks of a workspace yet"
            )),
            _ => None,
        }
    }

    /// Whether the build is lazy: by `--eager`, else by `RUSTD_LAZY`. A value that is none
    /// of theirs is the reason the command is refused.
    pub(crate) fn compilation(&self) -> Result<Compilation, String> {
        match &self.flags.compilation {
            Some(flag) => flag.clone(),
            None => Compilation::of_env(std::env::var(LAZY_ENV).ok().as_deref()),
        }
    }

    /// Why what was typed is not built, wherever it is typed: the flag, by its name.
    fn unbuilt_flag(&self) -> Option<String> {
        let flags = &self.flags;
        match self.compilation() {
            Err(why) => return Some(why),
            Ok(Compilation::Lazy) => {
                if let Some(why) = lazy_unsupported(self.target().as_deref()) {
                    return Some(why);
                }
            }
            Ok(Compilation::Eager) => {}
        }
        if let Some(flag) = &flags.plans {
            return Some(format!("`{flag}` is not answered by `cargo rustd` yet"));
        }
        if let Some(kind) =
            flags.message_format.iter().find(|it| !matches!(it.as_str(), "human" | "short"))
        {
            return Some(format!(
                "`--message-format={kind}` is not answered by `cargo rustd` yet: `human` and \
                 `short` are"
            ));
        }
        let reason = match () {
            _ if self.subcommand == Subcommand::Clippy
                && self.typed.iter().any(|it| it == "--fix") =>
            {
                "`--fix`: `cargo rustd clippy` does not change the source"
            }
            _ if flags.unstable => "`-Z`: unstable cargo flags are not read by rustd",
            _ if flags.targets.len() > 1 => {
                "several `--target` in one build are not built by the daemon yet"
            }
            _ => return self.target().and_then(|it| unsupported_target(&it)),
        };
        Some(reason.to_owned())
    }

    /// The profile, as cargo's `Finished` line names it.
    pub(crate) fn profile(&self) -> &str {
        let of_command = if self.subcommand == Subcommand::Test { "test" } else { "dev" };
        self.flags.profile.as_deref().unwrap_or(of_command)
    }

    /// Which build of the package the command is, as the daemon plans it. With no package
    /// named it is the one of the directory the worker is started in.
    pub(crate) fn selection(&self) -> Selection {
        Selection {
            profile: self.flags.profile.clone().unwrap_or_else(|| "dev".to_owned()),
            features: self.flags.features.clone(),
            no_default_features: self.flags.no_default_features,
            all_features: self.flags.all_features,
            packages: self.flags.packages.clone(),
            workspace: self.flags.workspace,
            excludes: self.flags.excludes.clone(),
            target: self.target(),
            dev_deps: self.subcommand == Subcommand::Test,
        }
    }

    /// The triple cargo builds for, when it is told one.
    pub(crate) fn target(&self) -> Option<String> {
        let told = std::env::var("CARGO_BUILD_TARGET").ok().filter(|it| !it.is_empty());
        self.flags.targets.first().cloned().or(told)
    }
}

impl Flags {
    fn read(&mut self, flag: &str, value: Option<&str>) {
        let owned = || value.map(str::to_owned);
        self.selects_targets |= TARGET_SELECTION.contains(&flag);
        match flag {
            "--manifest-path" => self.manifest_path = value.map(PathBuf::from),
            "-p" | "--package" => self.packages.extend(owned()),
            "--workspace" | "--all" => self.workspace = true,
            "--exclude" => self.excludes.extend(owned()),
            "-F" | "--features" => {
                let features = value.unwrap_or_default().split([',', ' ']);
                self.features.extend(features.filter(|it| !it.is_empty()).map(str::to_owned));
            }
            "--all-features" => self.all_features = true,
            "--no-default-features" => self.no_default_features = true,
            "-r" | "--release" => self.profile = Some("release".to_owned()),
            "--profile" => self.profile = owned(),
            "--target" => self.targets.extend(owned()),
            "--bin" => self.bin = owned(),
            "--example" => self.example = owned(),
            "--message-format" => {
                let kinds = value.unwrap_or_default().split(',').map(|it| it.trim().to_owned());
                self.message_format.extend(kinds);
            }
            "--build-plan" | "--unit-graph" => self.plans = Some(flag.to_owned()),
            "-Z" => self.unstable = true,
            "-q" | "--quiet" => self.quiet = true,
            "--tests" | "--test" | "--benches" | "--bench" | "--all-targets" => {
                self.tests = Some(flag.to_owned())
            }
            "--no-run" => self.no_run = true,
            "--no-fail-fast" => self.no_fail_fast = true,
            "--rerun-all" => self.rerun_all = true,
            "--doc" => self.doc = true,
            "--eager" => self.compilation = Some(Compilation::of_flag(value)),
            _ => {}
        }
    }
}

/// Whether the environment names a wrapper for the crates of the workspace that is not
/// rustd's.
fn another_wrapper() -> bool {
    let ours = [super::switch::wrapper_path().ok(), super::switch::first_wrapper_path().ok()];
    std::env::var_os(super::switch::VARIABLE)
        .is_some_and(|it| !it.is_empty() && !ours.iter().flatten().any(|ours| *ours == *it))
}

/// Whether the wrapper cargo is to run in place of rustc is where rustd looks for it.
/// Why cargo cannot be pointed at the wrappers, when one is not installed next to rustd:
/// `rustd-rustc` for the builds after the first, `rustd-rustc-first` for the first.
fn missing_wrapper() -> Option<String> {
    let wrappers = [super::switch::wrapper_path(), super::switch::first_wrapper_path()];
    let missing = wrappers.into_iter().find(|it| !it.as_ref().is_ok_and(|it| it.is_file()))?;
    let name = missing.ok().and_then(|it| Some(it.file_name()?.to_string_lossy().into_owned()));
    Some(format!("`{}` is not installed next to `rustd`", name.as_deref().unwrap_or("rustd-rustc")))
}

/// Why rustd does not build for `triple`, when it does not.
fn unsupported_target(triple: &str) -> Option<String> {
    use hir_ty::mir::codegen::{ObjectFormat, Target};
    match Target::select(triple) {
        Ok(target) if matches!(target.object_format, ObjectFormat::Elf | ObjectFormat::MachO) => {
            None
        }
        Ok(_) => Some(format!("programs for {triple} are not linked through `cargo rustd` yet")),
        Err(why) => Some(why),
    }
}

/// Why a lazy build is not made for `triple` (the host's when none), when it is not: the
/// table of stubs is written for ELF alone.
fn lazy_unsupported(triple: Option<&str>) -> Option<String> {
    use hir_ty::mir::codegen::{ObjectFormat, Target};
    let triple = triple.unwrap_or(Target::host());
    match Target::select(triple) {
        Ok(target) if matches!(target.object_format, ObjectFormat::Elf) => None,
        Ok(_) => Some(format!("a lazy build is made for ELF targets only, not {triple}")),
        Err(why) => Some(why),
    }
}

/// The variables of this process that are part of which build it is.
pub(crate) fn build_env() -> BTreeMap<String, String> {
    env_named(BUILD_ENV)
}

/// The variables of this process that a worker for its builds runs with.
pub(crate) fn worker_env() -> BTreeMap<String, String> {
    env_named(WORKER_ENV)
}

fn env_named(names: &[&str]) -> BTreeMap<String, String> {
    let named = |name: &str| {
        names.iter().any(|it| if it.ends_with('_') { name.starts_with(it) } else { name == *it })
    };
    std::env::vars().filter(|(name, _)| named(name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(subcommand: &str, args: &str) -> Invocation {
        let words = std::iter::once(subcommand).chain(args.split_whitespace());
        match Asked::parse(words.map(str::to_owned).collect()) {
            Asked::Build(invocation) => *invocation,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn flags_in_both_spellings() {
        let asked =
            build("build", "-p a --features=x,y -F z --target t --manifest-path m/Cargo.toml -r");
        assert_eq!(asked.flags.packages, ["a"]);
        assert_eq!(build("build", "-pa -j8").flags.packages, ["a"]);
        assert_eq!(asked.flags.features, ["x", "y", "z"]);
        assert_eq!(asked.flags.targets, ["t"]);
        assert_eq!(asked.flags.manifest_path.as_deref(), Some(Path::new("m/Cargo.toml")));
        assert_eq!(asked.flags.profile.as_deref(), Some("release"));
        assert_eq!(asked.program_args().count(), 0);
    }

    #[test]
    fn what_is_the_programs() {
        let run = build("run", "--bin a one -- --two");
        assert_eq!(run.build_args, ["--bin", "a"]);
        assert_eq!(run.program_args().collect::<Vec<_>>(), ["one", "--two"]);
        let test = build("test", "--no-fail-fast -p a filter --no-run -- --nocapture");
        assert_eq!(test.build_args, ["-p", "a"]);
        assert_eq!(
            (&*test.positional, &*test.trailing),
            (&["filter".to_owned()][..], &["--nocapture".to_owned()][..])
        );
        assert!(test.flags.no_fail_fast && test.flags.no_run);
    }

    #[test]
    fn a_flag_without_its_value_is_cargos_to_refuse() {
        let asked = build("build", "--features");
        assert_eq!(asked.build_args, ["--features"]);
        assert!(asked.flags.features.is_empty());
    }

    #[test]
    fn what_is_cargos_alone() {
        assert_eq!(Asked::parse(vec!["doc".to_owned()]), Asked::Cargo(vec!["doc".to_owned()]));
        assert_eq!(Asked::parse(Vec::new()), Asked::Help);
        assert_eq!(Asked::parse(vec!["--help".to_owned()]), Asked::Help);
        for args in [
            "--message-format json",
            "--message-format=short,json",
            "--unit-graph",
            "--target mips-unknown-none",
        ] {
            assert!(build("build", args).unbuilt_flag().is_some(), "{args}");
        }
        for args in [
            "--release",
            "--profile bench",
            "-p a -p b",
            "--all-features",
            "--no-default-features",
            "--workspace --exclude a",
        ] {
            assert_eq!(build("build", args).unbuilt_flag(), None, "{args}");
        }
        for (command, args) in
            [("build", "--all-targets"), ("test", "--benches"), ("test", "--bench a")]
        {
            assert!(build(command, args).builds_tests().is_some(), "{command} {args}");
        }
        for args in
            ["", "--bin a", "--example a", "--lib", "--bins", "--examples", "--test a", "--tests"]
        {
            assert_eq!(build("build", args).builds_tests(), None, "{args}");
        }
        for args in ["", "--test a", "--lib", "-p a --lib", "--tests"] {
            assert_eq!(build("test", args).builds_tests(), None, "{args}");
        }
        assert_eq!(build("test", "--release").profile(), "release");
        assert_eq!(build("test", "").profile(), "test");
    }

    #[test]
    fn run_is_the_build_that_build_is() {
        let cwd = Path::new("/ws");
        assert_eq!(build("run", "--bin a x -- y").key(cwd), build("build", "--bin a").key(cwd));
        assert_ne!(build("test", "--bin a").key(cwd), build("build", "--bin a").key(cwd));
        assert_ne!(build("build", "").key(cwd), build("build", "").key(Path::new("/other")));
        assert_ne!(build("build", "--features a").key(cwd), build("build", "").key(cwd));
    }

    #[test]
    fn eager_off_is_a_lazy_build_of_its_own_and_cargo_never_sees_the_flag() {
        let cwd = Path::new("/ws");
        let off = build("build", "-p a --eager off");
        assert_eq!(off.compilation(), Ok(Compilation::Lazy));
        assert_eq!(build("build", "--eager=off -p a").key(cwd), off.key(cwd));
        assert_ne!(off.key(cwd), build("build", "-p a").key(cwd));
        // `--eager on` is the eager build, whose key is the one it had before the flag.
        let on = build("build", "-p a --eager on");
        assert_eq!(on.compilation(), Ok(Compilation::Eager));
        assert_eq!(on.key(cwd), build("build", "-p a").key(cwd));
        for asked in [&off, &on, &build("run", "--eager=off -p a -- --eager on")] {
            assert_eq!(asked.build_args, ["-p", "a"], "{asked:?}");
            assert!(
                !asked.typed[..asked.typed.len() - asked.trailing.len()]
                    .iter()
                    .any(|it| it.starts_with("--eager") || it == "on" || it == "off")
            );
        }
        // What follows `--` is the program's, flags of ours or not.
        let run = build("run", "--eager=off -p a -- --eager on");
        assert_eq!(run.trailing, ["--eager", "on"]);
        assert_eq!(run.typed, ["-p", "a", "--", "--eager", "on"]);
    }

    #[test]
    fn rustd_lazy_asks_for_the_build_eager_off_does() {
        assert_eq!(Compilation::of_env(Some("1")), Compilation::of_flag(Some("off")));
        assert_eq!(Compilation::of_env(Some("0")), Ok(Compilation::Eager));
        assert_eq!(Compilation::of_env(Some("")), Ok(Compilation::Eager));
        assert_eq!(Compilation::of_env(None), Ok(Compilation::Eager));
        assert!(Compilation::of_env(Some("yes")).unwrap_err().contains("RUSTD_LAZY=yes"));
    }

    #[test]
    fn eager_with_another_value_is_refused() {
        let maybe = build("build", "--eager maybe");
        assert!(maybe.compilation().unwrap_err().contains("`--eager maybe`"));
        assert!(maybe.unbuilt_flag().is_some_and(|it| it.contains("`on` or `off`")));
        assert!(build("build", "--eager").compilation().is_err());
    }
}
