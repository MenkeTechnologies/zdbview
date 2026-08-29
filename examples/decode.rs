// Throwaway verification: decode a rkyv file and print detected format + records.
//
// The registry is re-included rather than linked (zdbview is a bin, not a lib),
// so the parts this example doesn't call — the edit/delete path in particular —
// register as dead code in this target only.
#[allow(dead_code)]
#[path = "../src/formats.rs"]
mod formats;
// formats resolves display names through the magic registry, so it comes along.
#[allow(dead_code)]
#[path = "../src/magics.rs"]
mod magics;

fn main() {
    let path = std::env::args().nth(1).expect("usage: decode <file>");
    let bytes = std::fs::read(&path).expect("read");
    match formats::try_decode(&bytes) {
        Some(d) => {
            println!("FORMAT: {}", d.format);
            println!("HEADER:");
            for (k, v) in &d.header {
                println!("  {k} = {v}");
            }
            println!("RECORDS: {}", d.records.len());
            for r in d.records.iter().take(5) {
                println!(
                    "  key={}  value={} bytes  fields={:?}",
                    r.key,
                    r.value.len(),
                    r.fields
                );
            }
        }
        None => println!("NOT RECOGNIZED (structural fallback)"),
    }
}
