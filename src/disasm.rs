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
