//! Lazy mode: a build that compiles only the roots a program needs before it runs, and
//! puts a stub in the object for every other root compiled code names. A stub is sixteen
//! bytes that load the stub's row in the table and raise `SIGILL`, so a call that reaches
//! one says which function it was. In M1 that is the whole story: the stubs abort, and
//! what they cost measures the ceiling of compiling on first call.
//!
//! The mode is a value that goes in at the top ([`Mode`] into `CodegenUnit::compile`) and
//! is matched where the lazy build differs; with `Mode::Eager` every such branch is what
//! the build did before the mode existed.

use std::path::PathBuf;

use hir_def::{
    FunctionId, attrs::AttrFlags, expr_store::Body, hir::Expr, signatures::FunctionSignature,
};
use rustc_hash::FxHashMap;

use super::{CodegenError, HirDatabase, Result};

/// Why a lazy build compiles a function before the program runs instead of stubbing it:
/// something other than compiled code of ours calls it, or its code is not ours to put
/// off. A function with more than one reason has the first in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EagerReason {
    /// The `main` the program starts in.
    Main,
    /// `#[no_mangle]` or `#[export_name]`: a runtime or a loader looks it up by name.
    Exported,
    /// A method of the `impl GlobalAlloc` that `#[global_allocator]` names: the program
    /// allocates through symbols rustc writes for it.
    Allocator,
    /// A `#[test]` the harness rustc wrote calls by its link name.
    Test,
    /// `#[link_section]`: the linker places it where a runtime finds it.
    Sectioned,
    /// Declared with an ABI other than Rust's: a callback given to C, as `signal`, `atexit`
    /// or a pthread take, or a constructor the loader runs.
    ForeignAbi,
    /// Its body has an `asm!` or `naked_asm!` block.
    Asm,
    /// `#[lang]`: the compiler's runtime names it.
    LangItem,
    /// A `#[used]` static, or one in a named section, holds its address: an `.init_array`
    /// entry, a registration a crate like `inventory` or `ctor` collects.
    PointedFromKeptStatic,
}

impl EagerReason {
    /// The name the report calls the reason by.
    pub fn as_str(self) -> &'static str {
        match self {
            EagerReason::Main => "main",
            EagerReason::Exported => "exported",
            EagerReason::Allocator => "allocator",
            EagerReason::Test => "test",
            EagerReason::Sectioned => "sectioned",
            EagerReason::ForeignAbi => "foreign_abi",
            EagerReason::Asm => "asm",
            EagerReason::LangItem => "lang_item",
            EagerReason::PointedFromKeptStatic => "pointed_from_kept_static",
        }
    }

    /// Every reason in the order one is chosen by.
    pub const ALL: [EagerReason; 9] = [
        EagerReason::Main,
        EagerReason::Exported,
        EagerReason::Allocator,
        EagerReason::Test,
        EagerReason::Sectioned,
        EagerReason::ForeignAbi,
        EagerReason::Asm,
        EagerReason::LangItem,
        EagerReason::PointedFromKeptStatic,
    ];

    /// The first reason a function's own declaration and body give, among the ones that
    /// are read off them alone: exported, in a section, of a foreign ABI, with inline
    /// assembly, or a lang item. The body is the lowered one, which a build has lowered
    /// already by the time it asks; closures share its expressions, so an `asm!` in one
    /// counts.
    pub fn declared(db: &dyn HirDatabase, func: FunctionId) -> Option<EagerReason> {
        if super::exported_symbol(db, func).is_some() {
            return Some(EagerReason::Exported);
        }
        if AttrFlags::link_section(db, func.into()).is_some() {
            return Some(EagerReason::Sectioned);
        }
        // The predicate `fn_abi` passes a call's values by: C, C-unwind, system, efiapi...
        if !FunctionSignature::of(db, func).abi.is_rustic_abi() {
            return Some(EagerReason::ForeignAbi);
        }
        if Body::of(db, func.into()).exprs().any(|(_, expr)| matches!(expr, Expr::InlineAsm(_))) {
            return Some(EagerReason::Asm);
        }
        if AttrFlags::query(db, func.into()).contains(AttrFlags::LANG_ITEM) {
            return Some(EagerReason::LangItem);
        }
        None
    }
}

/// The functions a lazy build compiles before the program runs, each with the first of its
/// reasons in [`EagerReason`]'s order, whichever order they were noted in.
#[derive(Default)]
pub struct EagerSet {
    reasons: FxHashMap<FunctionId, EagerReason>,
}

impl EagerSet {
    /// Notes a reason for `func`. One earlier in the order than the reason it has replaces
    /// it; a later one changes nothing.
    pub fn note(&mut self, func: FunctionId, reason: EagerReason) {
        self.reasons
            .entry(func)
            .and_modify(|known| *known = (*known).min(reason))
            .or_insert(reason);
    }

    pub fn reason(&self, func: FunctionId) -> Option<EagerReason> {
        self.reasons.get(&func).copied()
    }

    pub fn contains(&self, func: FunctionId) -> bool {
        self.reasons.contains_key(&func)
    }
}

/// The id of the build a daemon's worker serves: the sixteen bytes of `WorkerConfig::id`,
/// written as thirty-two hex digits. The table of stubs carries it, so that a process that
/// reads the table finds the daemon of its build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildId([u8; 16]);

impl BuildId {
    pub fn bytes(self) -> [u8; 16] {
        self.0
    }
}

impl std::str::FromStr for BuildId {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<BuildId, String> {
        let digits = text.as_bytes();
        if digits.len() != 32 {
            return Err(format!(
                "a build id is 32 hex digits, `{text}` is {} characters",
                digits.len()
            ));
        }
        let mut bytes = [0; 16];
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| format!("`{text}` is not hex"))?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| format!("`{text}` is not hex"))?;
        }
        Ok(BuildId(bytes))
    }
}

/// What the lazy build knows of the process it is in, for the table that a later milestone
/// reads to compile a stub's function on demand. A worker in driver mode, started by no
/// daemon, has neither a build id nor a socket.
pub struct LazyHost {
    pub build_id: Option<BuildId>,
    /// The socket the worker takes build requests on.
    pub socket: Option<PathBuf>,
    /// The worker's own executable.
    pub rustd: PathBuf,
    /// The directory of the store the worker compiles against, when it has one.
    pub store: Option<PathBuf>,
}

/// The inputs of a lazy build.
pub struct Lazy {
    /// The roots compiled before the program runs, with why each is, in the order they
    /// were listed. In lazy mode these are the entries; every other root is a stub.
    pub eager: Vec<(FunctionId, EagerReason)>,
    pub host: LazyHost,
}

/// Whether a build compiles every root it reaches, or only the eager ones with stubs for
/// the rest.
#[derive(Clone, Copy)]
pub enum Mode<'a> {
    Eager,
    Lazy(&'a Lazy),
}

/// The row of a stub in the object's table of stubs: dense from zero, in the order the
/// stubs were defined. Only [`Stubs::push`] makes one, so an index always names a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StubIndex(u32);

impl StubIndex {
    /// The row number, as the stub's code holds it.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// What the table says of one stub.
#[expect(dead_code, reason = "the table of stubs in the object reads the rows (M1 step 4)")]
pub(super) struct StubRow {
    pub(super) func: FunctionId,
    /// The rustc symbol the stub is defined under.
    pub(super) symbol: String,
    /// The function's stable name, when it has one.
    pub(super) name: Option<String>,
    /// The key of its entry in the store, when it has a stable name.
    pub(super) key: Option<[u8; 32]>,
}

/// The stubs of one object, in the order they were defined.
#[derive(Default)]
pub(super) struct Stubs {
    rows: Vec<StubRow>,
}

impl Stubs {
    /// Adds a row and says which it is. Refuses once the row number no longer fits the
    /// stub's code.
    pub(super) fn push(&mut self, row: StubRow) -> Result<StubIndex> {
        let index = u32::try_from(self.rows.len())
            .map_err(|_| CodegenError::Backend("more stubs than a stub can name".to_owned()))?;
        self.rows.push(row);
        Ok(StubIndex(index))
    }

    #[expect(dead_code, reason = "the table of stubs in the object reads the rows (M1 step 4)")]
    pub(super) fn rows(&self) -> &[StubRow] {
        &self.rows
    }
}

#[cfg(test)]
mod tests {
    use salsa::plumbing::FromId as _;

    use super::{super::Arch, BuildId, EagerReason, EagerSet, StubIndex};
    use hir_def::FunctionId;

    /// A function id that is nothing but a number: the set never asks the database.
    fn function(n: u64) -> FunctionId {
        FunctionId::from_id(salsa::Id::from_bits(n))
    }

    #[test]
    fn the_first_reason_in_order_wins_whichever_is_noted_first() {
        let mut set = EagerSet::default();
        let f = function(1);
        set.note(f, EagerReason::ForeignAbi);
        set.note(f, EagerReason::Exported);
        set.note(f, EagerReason::Asm);
        assert_eq!(set.reason(f), Some(EagerReason::Exported));
        // `main` beats every other reason; a kept static's pointer beats none.
        set.note(f, EagerReason::PointedFromKeptStatic);
        assert_eq!(set.reason(f), Some(EagerReason::Exported));
        set.note(f, EagerReason::Main);
        assert_eq!(set.reason(f), Some(EagerReason::Main));
        let g = function(2);
        set.note(g, EagerReason::PointedFromKeptStatic);
        set.note(g, EagerReason::LangItem);
        set.note(g, EagerReason::Test);
        assert_eq!(set.reason(g), Some(EagerReason::Test));
        assert!(!set.contains(function(3)));
        assert_eq!(set.reason(function(3)), None);
    }

    #[test]
    fn the_order_of_reasons_is_the_order_they_are_declared_in() {
        let all = EagerReason::ALL;
        assert!(all.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(all[0], EagerReason::Main);
        assert_eq!(all[8], EagerReason::PointedFromKeptStatic);
        let names: Vec<_> = all.iter().map(|it| it.as_str()).collect();
        assert_eq!(names.len(), {
            let mut unique = names.clone();
            unique.sort();
            unique.dedup();
            unique.len()
        });
    }

    #[test]
    fn a_build_id_is_thirty_two_hex_digits() {
        let id: BuildId = "00112233445566778899aabbccddeeff".parse().unwrap();
        assert_eq!(
            id.bytes(),
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ]
        );
        assert!("0011".parse::<BuildId>().is_err());
        assert!("zz112233445566778899aabbccddeeff".parse::<BuildId>().is_err());
        assert!("00112233445566778899aabbccddeeff0".parse::<BuildId>().is_err());
    }

    fn x86_64(index: u32) -> [u8; 16] {
        let [a, b, c, d] = index.to_le_bytes();
        [0xb8, a, b, c, d, 0x0f, 0x0b, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc]
    }

    fn aarch64(words: [u32; 4]) -> [u8; 16] {
        let mut bytes = [0; 16];
        for (at, word) in words.iter().enumerate() {
            bytes[at * 4..at * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn x86_64_stub_holds_the_index_then_traps() {
        for index in [0, 1, 0xffff, 0x12345678] {
            assert_eq!(
                Arch::X86_64.stub(StubIndex(index)),
                Some(x86_64(index)),
                "index {index:#x}"
            );
        }
        // `mov eax, 0` is as long as the jump a replacement is written over it with.
        assert_eq!(Arch::X86_64.shortest_function(), Some((5, 0xcc)));
    }

    #[test]
    fn aarch64_stub_holds_the_index_then_traps() {
        let nop = 0xd503201f;
        let cases = [
            (0, [0x52800010, 0x72a00010, 0, nop]),
            (1, [0x52800030, 0x72a00010, 0, nop]),
            (0xffff, [0x529ffff0, 0x72a00010, 0, nop]),
            (0x12345678, [0x528acf10, 0x72a24690, 0, nop]),
        ];
        for (index, words) in cases {
            assert_eq!(
                Arch::Aarch64.stub(StubIndex(index)),
                Some(aarch64(words)),
                "index {index:#x}"
            );
        }
    }

    #[test]
    fn wasm_has_no_stub() {
        assert_eq!(Arch::Wasm32.stub(StubIndex(0)), None);
    }
}
