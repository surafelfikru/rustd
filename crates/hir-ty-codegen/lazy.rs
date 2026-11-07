//! Lazy mode: a build that compiles only the roots a program needs before it runs, and
//! puts a stub in the object for every other root compiled code names. A stub is sixteen
//! bytes that load the stub's row in the table and raise `SIGILL`, so a call that reaches
//! one says which function it was. In M1 that is the whole story: the stubs abort, and
//! what they cost measures the ceiling of compiling on first call.
//!
//! The mode is a value that goes in at the top ([`Mode`] into `CodegenUnit::compile`) and
//! is matched where the lazy build differs; with `Mode::Eager` every such branch is what
//! the build did before the mode existed.

use hir_def::FunctionId;

use super::{CodegenError, Result};

/// What the lazy build knows of the process it is in, for the table that a later milestone
/// reads to compile a stub's function on demand. Its fields come with the eager set.
pub struct LazyHost;

/// The inputs of a lazy build.
pub struct Lazy {
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
    use super::{super::Arch, StubIndex};

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
