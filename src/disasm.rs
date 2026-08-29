//! Bytecode disassembly of `fusevm::Chunk` value blobs (feature = "disasm").
//!
//! The script-cache value blobs (Family A + stryke `chunk_blob`) are
//! `bincode`-encoded `fusevm::Chunk`. Rather than vendor a copy of the
//! `Op` enum — whose variant list grows every release, and which would silently
//! misdecode the moment fusevm's variant order drifts — this uses the real
//! `fusevm` types, so the output is always correct or fails loudly. Blobs that
//! are not a bare `Chunk` (pythonrs `CProg`, elisprs heap image, …) fail to
//! decode and fall back to hex.

use fusevm::Chunk;

/// Decode `bytes` as a `fusevm::Chunk` and render a human-readable listing.
/// Returns an error string if the blob is not a bare bincode `Chunk`.
pub fn disassemble(bytes: &[u8]) -> Result<Vec<String>, String> {
    let chunk: Chunk = bincode::deserialize(bytes).map_err(|e| e.to_string())?;
    Ok(render(&chunk, 0))
}

fn render(c: &Chunk, depth: usize) -> Vec<String> {
    let ind = "  ".repeat(depth);
    let mut out = Vec::new();
    out.push(format!("{ind}source: {}", c.source));
    out.push(format!(
        "{ind}{} ops · {} constants · {} names · {} sub-chunks",
        c.ops.len(),
        c.constants.len(),
        c.names.len(),
        c.sub_chunks.len()
    ));

    if !c.names.is_empty() {
        out.push(format!("{ind}names:"));
        for (i, n) in c.names.iter().enumerate() {
            out.push(format!("{ind}  [{i}] {n}"));
        }
    }
    if !c.constants.is_empty() {
        out.push(format!("{ind}constants:"));
        for (i, v) in c.constants.iter().enumerate() {
            out.push(format!("{ind}  [{i}] {v:?}"));
        }
    }

    out.push(format!("{ind}ops:"));
    for (i, op) in c.ops.iter().enumerate() {
        let line = c.lines.get(i).copied().unwrap_or(0);
        out.push(format!("{ind}  {i:>5}  L{line:<5} {op:?}"));
    }

    // Nested chunks (command substitutions, process subs, trap/function bodies).
    for (idx, sub) in c.sub_chunks.iter().enumerate() {
        out.push(String::new());
        out.push(format!("{ind}── sub_chunk[{idx}] ──"));
        out.extend(render(sub, depth + 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::disassemble;
    use fusevm::{Chunk, Op, Value};

    /// A chunk with one of everything the listing prints, including a nested
    /// chunk — a command substitution's body, as the shell compiles one.
    fn chunk() -> Chunk {
        let mut inner = Chunk::new();
        inner.source = "inner.sh".into();
        inner.ops = vec![Op::Nop];
        inner.lines = vec![7];

        let mut c = Chunk::new();
        c.source = "outer.sh".into();
        c.names = vec!["PATH".into(), "greet".into()];
        c.constants = vec![Value::Int(42), Value::Str(std::sync::Arc::new("hi".into()))];
        c.ops = vec![Op::LoadConst(0), Op::LoadInt(9), Op::Nop];
        c.lines = vec![1, 2, 3];
        c.sub_chunks = vec![inner];
        c
    }

    #[test]
    fn a_listing_names_every_pool_and_numbers_every_op() {
        let bytes = bincode::serialize(&chunk()).unwrap();
        let lines = disassemble(&bytes).expect("a bare chunk decodes");
        let text = lines.join("\n");

        assert!(text.contains("source: outer.sh"), "{text}");
        assert!(
            text.contains("3 ops · 2 constants · 2 names · 1 sub-chunks"),
            "the header counts each pool: {text}"
        );
        // Pools are indexed, because an op refers to them by index.
        assert!(
            text.contains("[0] PATH") && text.contains("[1] greet"),
            "{text}"
        );
        assert!(text.contains("[0] Int(42)"), "{text}");
        // Ops carry their index and their source line, which is what makes a
        // listing traceable back to the script.
        assert!(text.contains("0  L1     LoadConst(0)"), "{text}");
        assert!(text.contains("2  L3     Nop"), "{text}");

        // The nested chunk is listed under its own heading and indented.
        assert!(text.contains("── sub_chunk[0] ──"), "{text}");
        assert!(text.contains("  source: inner.sh"), "indented: {text}");
        assert!(text.contains("  L7"), "and keeps its own lines: {text}");
    }

    /// The blobs that are not a bare `Chunk` — pythonrs `CProg`, an elisprs heap
    /// image, a truncated write — have to fail rather than render nonsense, since
    /// the caller falls back to hex on an error.
    #[test]
    fn anything_that_is_not_a_chunk_is_refused() {
        assert!(disassemble(b"").is_err(), "nothing is not a chunk");
        assert!(disassemble(b"not bincode at all").is_err());

        // A chunk cut short mid-encoding is the realistic corruption, and it must
        // not decode to a shorter listing.
        let bytes = bincode::serialize(&chunk()).unwrap();
        assert!(
            disassemble(&bytes[..bytes.len() / 2]).is_err(),
            "half a chunk is not a chunk"
        );
        // An error says what went wrong rather than being empty.
        assert!(!disassemble(b"x").unwrap_err().is_empty());
    }
}
