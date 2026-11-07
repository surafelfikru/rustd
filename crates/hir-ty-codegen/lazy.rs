//! Lazy mode: a build that compiles only the roots a program needs before it runs, and
//! puts a stub in the object for every other root compiled code names. A stub is sixteen
//! bytes that load the stub's row in the table and raise `SIGILL`, so a call that reaches
//! one says which function it was. In M1 that is the whole story: the stubs abort, and
//! what they cost measures the ceiling of compiling on first call.
//!
//! The mode is a value that goes in at the top ([`Mode`] into `CodegenUnit::compile`) and
//! is matched where the lazy build differs; with `Mode::Eager` every such branch is what
//! the build did before the mode existed.

use std::path::{Path, PathBuf};

use cranelift_module::FuncId;
use cranelift_object::{
    ObjectProduct,
    object::{
        BinaryFormat, RelocationEncoding, RelocationFlags, RelocationKind, SectionFlags,
        SectionKind, SymbolFlags, SymbolKind, SymbolScope, elf,
        write::{Relocation, Symbol, SymbolSection},
    },
};
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
pub(super) struct StubRow {
    #[expect(dead_code, reason = "compiling a stub's function on its first call reads it (M2)")]
    pub(super) func: FunctionId,
    /// The stub as the module declared it, which the table's row points at.
    pub(super) id: FuncId,
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

    pub(super) fn rows(&self) -> &[StubRow] {
        &self.rows
    }
}

/// The section a lazy object's table of stubs is in.
pub const TABLE_SECTION: &str = ".rustd.lazy";
/// The symbol at the start of the table, hidden from everything outside the program.
pub const TABLE_SYMBOL: &str = "rustd_lazy_table";
const MAGIC: [u8; 8] = *b"rustdlzy";
const VERSION: u32 = 1;
const HEADER: usize = 96;
const ROW: usize = 16;
const NO_KEY: u32 = u32::MAX;

/// The table of a lazy object's stubs, which a process reads to find the daemon of its
/// build and the function a stub stands in for, and so compile that function on its first
/// call. One table per program, in the section [`TABLE_SECTION`] under [`TABLE_SYMBOL`].
///
/// It is little-endian and aligned to eight. A header of 96 bytes:
///
/// ```text
/// magic "rustdlzy" [8]  version u32  count u32  build_id [16]  program [32]
/// socket (off u32, len u32)  rustd (off, len)  store (off, len)  rows_off u32  pool_off u32
/// ```
///
/// then `count` rows of 16 bytes, one per stub in the order of their indices:
///
/// ```text
/// stub i32  name_off u32  name_len u32  key_off u32
/// ```
///
/// then a pool of bytes the offsets point into, counted from the pool's start. `stub` is the
/// stub's address less the field's own, so the table needs no relocation once linked. A
/// build id or program of all zeros is none; a path or name of length zero is none; a key
/// offset of `u32::MAX` is none, else it points at the 32 bytes of the stub's store key.
pub(super) struct LazyTable {
    bytes: Vec<u8>,
    /// The offset of each row's `stub` field, with the stub it points at.
    stubs: Vec<(u64, FuncId)>,
}

impl LazyTable {
    pub(super) fn new(host: &LazyHost, stubs: &Stubs, program: Option<&[u8; 32]>) -> LazyTable {
        let mut pool = Vec::new();
        let mut place = |bytes: &[u8]| {
            let at = pool.len() as u32;
            pool.extend_from_slice(bytes);
            (at, bytes.len() as u32)
        };
        fn bytes(path: &Path) -> &[u8] {
            path.as_os_str().as_encoded_bytes()
        }
        let socket = host.socket.as_deref().map_or((0, 0), |it| place(bytes(it)));
        let rustd = place(bytes(&host.rustd));
        let store = host.store.as_deref().map_or((0, 0), |it| place(bytes(it)));
        let rows: Vec<_> = stubs
            .rows()
            .iter()
            .map(|row| {
                let (name_off, name_len) =
                    row.name.as_deref().map_or((0, 0), |it| place(it.as_bytes()));
                let key_off = row.key.as_ref().map_or(NO_KEY, |it| place(it).0);
                (row.id, name_off, name_len, key_off)
            })
            .collect();
        let rows_off = HEADER;
        let pool_off = rows_off + rows.len() * ROW;
        let mut bytes = Vec::with_capacity(pool_off + pool.len());
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&(rows.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&host.build_id.map_or([0; 16], BuildId::bytes));
        bytes.extend_from_slice(program.unwrap_or(&[0; 32]));
        for (off, len) in [socket, rustd, store] {
            bytes.extend_from_slice(&off.to_le_bytes());
            bytes.extend_from_slice(&len.to_le_bytes());
        }
        bytes.extend_from_slice(&(rows_off as u32).to_le_bytes());
        bytes.extend_from_slice(&(pool_off as u32).to_le_bytes());
        debug_assert_eq!(bytes.len(), HEADER);
        let mut placed = Vec::with_capacity(rows.len());
        for (id, name_off, name_len, key_off) in rows {
            placed.push((bytes.len() as u64, id));
            // The stub's distance from here, which the linker writes.
            bytes.extend_from_slice(&0i32.to_le_bytes());
            for field in [name_off, name_len, key_off] {
                bytes.extend_from_slice(&field.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&pool);
        LazyTable { bytes, stubs: placed }
    }

    /// Puts the table in the object, kept by a linker that collects unused sections, with a
    /// relocation per row that makes its `stub` field the stub's distance from the field.
    pub(super) fn place(self, product: &mut ObjectProduct) -> std::result::Result<(), String> {
        if product.object.format() != BinaryFormat::Elf {
            return Err("lazy mode: the table of stubs is written for ELF only".to_owned());
        }
        let len = self.bytes.len() as u64;
        let object = &mut product.object;
        let section = object.add_section(
            Vec::new(),
            TABLE_SECTION.as_bytes().to_vec(),
            SectionKind::ReadOnlyData,
        );
        object.section_mut(section).set_data(self.bytes, 8);
        if let SectionFlags::Elf { sh_flags, .. } = object.section_flags_mut(section) {
            *sh_flags |= elf::SHF_GNU_RETAIN;
        }
        for (offset, stub) in self.stubs {
            let symbol = product.function_symbol(stub);
            // As the unwind tables do: a local symbol is addressed through its section.
            let object = &mut product.object;
            let (symbol, addend) = object.symbol_section_and_offset(symbol).unwrap_or((symbol, 0));
            let flags = RelocationFlags::Generic {
                kind: RelocationKind::Relative,
                encoding: RelocationEncoding::Generic,
                size: 32,
            };
            let reloc = Relocation { offset, symbol, flags, addend: addend as i64 };
            object.add_relocation(section, reloc).map_err(|e| e.to_string())?;
        }
        product.object.add_symbol(Symbol {
            name: TABLE_SYMBOL.as_bytes().to_vec(),
            value: 0,
            size: len,
            kind: SymbolKind::Data,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        });
        Ok(())
    }
}

/// A table of stubs as a process reads it, from the bytes at [`TABLE_SYMBOL`].
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedTable<'a> {
    pub build_id: Option<[u8; 16]>,
    pub program: Option<[u8; 32]>,
    pub socket: Option<&'a [u8]>,
    pub rustd: &'a [u8],
    pub store: Option<&'a [u8]>,
    pub rows: Vec<ParsedRow<'a>>,
}

/// One stub of a [`ParsedTable`].
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedRow<'a> {
    /// The stub's address less the address of this row's first field.
    pub stub: i32,
    /// Where in the table this row starts, which `stub` is counted from.
    pub at: usize,
    pub name: Option<&'a str>,
    pub key: Option<[u8; 32]>,
}

impl<'a> ParsedTable<'a> {
    /// Reads a table, refusing one of another layout or one whose offsets point past its end.
    pub fn parse(bytes: &'a [u8]) -> std::result::Result<ParsedTable<'a>, String> {
        let short = || format!("a table of stubs is cut short at {} bytes", bytes.len());
        let take = |at: usize, len: usize| bytes.get(at..at + len).ok_or_else(short);
        let word = |at: usize| take(at, 4).map(|it| u32::from_le_bytes(it.try_into().unwrap()));
        if take(0, 8)? != MAGIC {
            return Err("not a table of stubs: the magic is not `rustdlzy`".to_owned());
        }
        let version = word(8)?;
        if version != VERSION {
            return Err(format!("a table of stubs of version {version}, this reads {VERSION}"));
        }
        let count = word(12)? as usize;
        let nonzero = |it: &[u8]| it.iter().any(|b| *b != 0);
        let build_id =
            Some(take(16, 16)?).filter(|it| nonzero(it)).map(|it| it.try_into().unwrap());
        let program = Some(take(32, 32)?).filter(|it| nonzero(it)).map(|it| it.try_into().unwrap());
        let (rows_off, pool_off) = (word(88)? as usize, word(92)? as usize);
        let pooled = |off: u32, len: u32| take(pool_off + off as usize, len as usize);
        let path = |at: usize| -> std::result::Result<Option<&'a [u8]>, String> {
            let (off, len) = (word(at)?, word(at + 4)?);
            Ok(if len == 0 { None } else { Some(pooled(off, len)?) })
        };
        let (socket, store) = (path(64)?, path(80)?);
        let rustd = path(72)?.ok_or("a table of stubs names no rustd")?;
        let mut rows = Vec::with_capacity(count);
        for index in 0..count {
            let at = rows_off + index * ROW;
            let stub = word(at)? as i32;
            let (name_off, name_len, key_off) = (word(at + 4)?, word(at + 8)?, word(at + 12)?);
            let name = match name_len {
                0 => None,
                _ => Some(
                    std::str::from_utf8(pooled(name_off, name_len)?)
                        .map_err(|_| format!("the name of stub {index} is not UTF-8"))?,
                ),
            };
            let key = match key_off {
                NO_KEY => None,
                _ => Some(pooled(key_off, 32)?.try_into().unwrap()),
            };
            rows.push(ParsedRow { stub, at, name, key });
        }
        Ok(ParsedTable { build_id, program, socket, rustd, store, rows })
    }
}

#[cfg(test)]
mod tests {
    use salsa::plumbing::FromId as _;

    use super::{
        super::Arch, BuildId, EagerReason, EagerSet, LazyHost, LazyTable, ParsedTable, StubIndex,
        StubRow, Stubs,
    };
    use cranelift_module::FuncId;
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

    fn three_stubs() -> Stubs {
        let mut stubs = Stubs::default();
        let rows =
            [(Some("fn app::parse"), Some([7; 32])), (None, None), (Some("fn app::render"), None)];
        for (n, (name, key)) in rows.into_iter().enumerate() {
            let row = StubRow {
                func: function(n as u64 + 1),
                id: FuncId::from_u32(n as u32),
                name: name.map(str::to_owned),
                key,
            };
            assert_eq!(stubs.push(row).unwrap().get(), n as u32);
        }
        stubs
    }

    #[test]
    fn a_table_reads_back_as_it_was_written() {
        let host = LazyHost {
            build_id: Some("00112233445566778899aabbccddeeff".parse().unwrap()),
            socket: Some("/run/rustd/worker.sock".into()),
            rustd: "/usr/bin/rustd".into(),
            store: None,
        };
        let table = LazyTable::new(&host, &three_stubs(), Some(&[9; 32]));
        // Each row's stub field is where the linker is told to write the distance.
        let offsets: Vec<_> = table.stubs.iter().map(|it| it.0).collect();
        assert_eq!(offsets, [96, 112, 128]);
        let parsed = ParsedTable::parse(&table.bytes).unwrap();
        assert_eq!(parsed.build_id, Some(host.build_id.unwrap().bytes()));
        assert_eq!(parsed.program, Some([9; 32]));
        assert_eq!(parsed.socket, Some(&b"/run/rustd/worker.sock"[..]));
        assert_eq!(parsed.rustd, b"/usr/bin/rustd");
        assert_eq!(parsed.store, None);
        let rows: Vec<_> = parsed.rows.iter().map(|it| (it.stub, it.at, it.name, it.key)).collect();
        assert_eq!(
            rows,
            [
                (0, 96, Some("fn app::parse"), Some([7; 32])),
                (0, 112, None, None),
                (0, 128, Some("fn app::render"), None),
            ]
        );
    }

    #[test]
    fn a_table_of_a_worker_no_daemon_started_has_no_build_id_or_socket() {
        let host =
            LazyHost { build_id: None, socket: None, rustd: "/usr/bin/rustd".into(), store: None };
        let table = LazyTable::new(&host, &Stubs::default(), None);
        let parsed = ParsedTable::parse(&table.bytes).unwrap();
        assert_eq!((parsed.build_id, parsed.program, parsed.socket), (None, None, None));
        assert!(parsed.rows.is_empty());
    }

    #[test]
    fn a_table_of_another_layout_is_refused() {
        let host =
            LazyHost { build_id: None, socket: None, rustd: "/usr/bin/rustd".into(), store: None };
        let bytes = LazyTable::new(&host, &three_stubs(), None).bytes;
        let mut magic = bytes.clone();
        magic[0] = b'R';
        assert!(ParsedTable::parse(&magic).unwrap_err().contains("magic"));
        let mut version = bytes.clone();
        version[8] = 2;
        assert!(ParsedTable::parse(&version).unwrap_err().contains("version 2"));
        assert!(ParsedTable::parse(&bytes[..bytes.len() - 1]).unwrap_err().contains("cut short"));
        assert!(ParsedTable::parse(&bytes[..40]).unwrap_err().contains("cut short"));
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
