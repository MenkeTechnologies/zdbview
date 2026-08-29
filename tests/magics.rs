//! Proof that the magic registry is open to anyone.
//!
//! The claim these tests defend is the one `spec/rkyv-shard-header.md` makes:
//! tags belong to producers, not to this reader, so a producer zdbview has never
//! heard of registers a tag by publishing it — no patch, no release, no entry in
//! anyone's source. Here that producer is `LUAR`, which zdbview does not ship; a
//! user writes it into their registry file and every path the viewer uses to
//! name a format then names it.

use std::io::Write;

#[allow(dead_code)]
#[path = "../src/formats.rs"]
mod formats;
#[allow(dead_code)]
#[path = "../src/magics.rs"]
mod magics;

/// A tag no zdbview build knows: a hypothetical `luars` bytecode cache.
const LUAR: u32 = 0x4C55_4152;
/// A second, registered by explicit hex rather than by its four characters.
const PERL: u32 = 0x5045_524C;
/// A tag zdbview does ship, which the same file renames.
const VIML: u32 = 0x5649_4D4C;

const USER_FILE: &str = "\
# a producer this build has never heard of
LUAR = luars bytecode cache (LUAR)
0x5045_524C = perlrs script cache (PERL)
VIML = vimlrs script cache, renamed by its user
";

/// Install the registry the file above describes, once for this binary. Every
/// test calls it, so whichever runs first resolves the registry and the rest see
/// the same one — the process registry resolves exactly once by design.
fn registry() -> &'static [(u32, &'static str)] {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut path = std::env::temp_dir();
        path.push(format!("zdbview_magics_{}", std::process::id()));
        let mut f = std::fs::File::create(&path).expect("write the registry file");
        f.write_all(USER_FILE.as_bytes()).expect("write");
        drop(f);
        // Read back through the real file-reading path, then install it as the
        // registry `formats` resolves names through.
        assert!(magics::install(magics::registry_for(&path)), "installed");
        let _ = std::fs::remove_file(&path);
    });
    magics::all()
}

/// A shard-shaped buffer: the tag little-endian, where a producer's header puts
/// it, with rkyv's root trailing it.
fn stamped(magic: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 32];
    bytes.extend_from_slice(&magic.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 64]);
    bytes
}

#[test]
fn a_tag_this_build_never_shipped_is_detected_and_named() {
    // The premise: LUAR is not zdbview's. Nothing in the binary knows it.
    assert!(
        !formats::BUILTIN_MAGICS.iter().any(|(m, _)| *m == LUAR),
        "LUAR must not be a built-in, or this proves nothing"
    );
    assert!(registry().iter().any(|(m, _)| *m == LUAR));

    // The sniff the startup scan runs over a file's bytes now names it.
    assert_eq!(
        formats::magic_in(&stamped(LUAR)),
        Some("luars bytecode cache (LUAR)")
    );
    // And so does the hex form of a tag, for producers whose tag is not four
    // printable characters.
    assert_eq!(
        formats::magic_in(&stamped(PERL)),
        Some("perlrs script cache (PERL)")
    );
    // Naming is all a registration buys, and all it should: with no decoder for
    // the tag, the archive still falls through to the structural view rather
    // than being cast to some other producer's type.
    assert!(formats::try_decode(&stamped(LUAR)).is_none());
}

#[test]
fn a_user_tag_survives_the_scan_cache_round_trip() {
    registry();
    // The saved scan stores the display name and restores the format from it;
    // a user tag has to come back through that lookup like any built-in.
    let name = formats::magic_in(&stamped(LUAR)).expect("detected");
    assert_eq!(formats::magic_label(name), Some("luars bytecode cache (LUAR)"));
    assert_eq!(formats::magic_label("a format nobody registered"), None);
}

#[test]
fn a_user_entry_renames_a_builtin_tag() {
    registry();
    assert_eq!(
        formats::magic_in(&stamped(VIML)),
        Some("vimlrs script cache, renamed by its user")
    );
    // Renaming is a display change only: the tag is still VIML, listed once.
    assert_eq!(registry().iter().filter(|(m, _)| *m == VIML).count(), 1);
}

#[test]
fn registering_a_tag_does_not_disturb_the_built_ins() {
    let reg = registry();
    for (magic, name) in formats::BUILTIN_MAGICS {
        let got = magics::name_of(*magic).expect("every built-in is still registered");
        if *magic == VIML {
            continue; // renamed above, on purpose
        }
        assert_eq!(got, *name, "built-in {magic:#010x} kept its name");
    }
    assert_eq!(reg.len(), formats::BUILTIN_MAGICS.len() + 2);
}

#[test]
fn the_registry_file_is_the_documented_path() {
    // Where a user is told to put the file, in README and the man page.
    let prior = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::temp_dir().join("zdbview_home_probe");
    std::env::set_var("XDG_CONFIG_HOME", &home);
    let got = magics::registry_path();
    match prior {
        Some(p) => std::env::set_var("XDG_CONFIG_HOME", p),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    assert_eq!(got, Some(home.join("zdbview").join("magics")));
}

/// The proof that owes nothing to a test seam: the shipped binary, a config file
/// a user wrote, no code change. Everything above installs the registry through
/// `magics::install`, which only a test can call — this runs `zdbview --formats`
/// as a user would, with `XDG_CONFIG_HOME` pointed at a directory holding one
/// hand-written line, and reads what the binary prints.
#[test]
fn the_shipped_binary_honours_a_hand_written_registry_file() {
    let dir = std::env::temp_dir().join(format!("zdbview_cfg_{}", std::process::id()));
    let zdb = dir.join("zdbview");
    std::fs::create_dir_all(&zdb).expect("config dir");
    std::fs::write(zdb.join("magics"), USER_FILE).expect("registry file");

    let run = |config: &std::path::Path| -> String {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_zdbview"))
            .arg("--formats")
            .env("XDG_CONFIG_HOME", config)
            .output()
            .expect("run zdbview --formats");
        assert!(out.status.success(), "--formats exits 0");
        // The listing is coloured for a terminal; the escapes are not the point.
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        strip_ansi(&text)
    };

    let listed = run(&dir);
    assert!(
        listed.contains("LUAR  0x4c554152  luars bytecode cache (LUAR)  (registered)"),
        "a tag the binary does not ship, listed from the user's file:\n{listed}"
    );
    assert!(
        listed.contains("PERL  0x5045524c  perlrs script cache (PERL)  (registered)"),
        "a tag written as hex, listed by its characters:\n{listed}"
    );
    assert!(
        listed.contains("VIML  0x56494d4c  vimlrs script cache, renamed by its user"),
        "a shipped tag the user renamed:\n{listed}"
    );

    // Control: the same binary, a config directory with no registry file. What
    // the file added is exactly what disappears.
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).expect("empty config dir");
    let bare = run(&empty);
    assert!(!bare.contains("LUAR"), "nothing registers itself:\n{bare}");
    assert!(!bare.contains("(registered)"), "no strays:\n{bare}");
    assert!(
        bare.contains("VIML  0x56494d4c  vimlrs script cache (VIML)"),
        "the built-in name, unrenamed:\n{bare}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Colour codes out, so an assertion reads the text and not the styling.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
