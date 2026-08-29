//! The magic registry: four-byte producer tags and their display names.
//!
//! zdbview ships the tags it knows, but the header format is a published
//! convention (`docs/rkyv-shard-header.md`), not a zdbview-private one — so any
//! producer can stamp its own tag and have it named here without patching this
//! binary. Extra tags are read from `$XDG_CONFIG_HOME/zdbview/magics` (falling
//! back to `~/.config/zdbview/magics`), one per line:
//!
//! ```text
//! # tag = display name
//! LUAR = luars bytecode cache (LUAR)
//! 0x5045_524C = perlrs script cache (PERL)
//! ```
//!
//! A four-character tag is read as the little-endian u32 a producer writing
//! `u32::from_be_bytes(*b"LUAR")` stamps — i.e. the bytes appear in file order.
//! Hex (`0x…`, underscores allowed) sets the u32 directly for anything that is
//! not four printable ASCII characters.
//!
//! A user entry for a tag zdbview already knows overrides its name; the decoders
//! keyed on that tag are unaffected. A tag with no decoder is still worth
//! registering: the scan then offers the file under its real name, and the
//! structural inspector opens it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Registry entries, resolved once: builtins overlaid with the user's file. The
/// names are leaked so callers keep the `&'static str` they had before the
/// registry became extensible; the set is small and read once per process.
static REGISTRY: OnceLock<Vec<(u32, &'static str)>> = OnceLock::new();

/// A registry a test installed, which wins over the resolved one.
static OVERRIDE: OnceLock<Vec<(u32, &'static str)>> = OnceLock::new();

/// Every known tag with its display name, user entries included.
///
/// Under `cfg(test)` the user's own file is skipped and only the built-ins are
/// resolved, so a developer who registered a tag on their machine cannot change
/// what the suite asserts. Tests that want a registry install one explicitly
/// with [`install`].
pub fn all() -> &'static [(u32, &'static str)] {
    if let Some(installed) = OVERRIDE.get() {
        return installed;
    }
    REGISTRY.get_or_init(|| {
        if cfg!(test) {
            crate::formats::BUILTIN_MAGICS.to_vec()
        } else {
            registry_for_entries(&user_entries())
        }
    })
}

/// The built-ins overlaid with the registry file at `path` — what the running
/// binary resolves, addressable by path so a test can point it at a file it
/// wrote instead of the user's.
// Used by tests/magics.rs, which re-includes this module rather than linking it.
#[allow(dead_code)]
pub(crate) fn registry_for(path: &Path) -> Vec<(u32, &'static str)> {
    registry_for_entries(&parse_file(path))
}

fn registry_for_entries(user: &[(u32, String)]) -> Vec<(u32, &'static str)> {
    merge(crate::formats::BUILTIN_MAGICS, user)
}

/// Install `entries` as the registry [`all`] returns, for a test that needs
/// everything reading through it to see a registry it controls. Takes precedence
/// over the resolved one, so it holds whether or not something already read the
/// registry. `false` if a registry was already installed.
#[allow(dead_code)]
pub(crate) fn install(entries: Vec<(u32, &'static str)>) -> bool {
    OVERRIDE.set(entries).is_ok()
}

/// Display name for `magic`, or `None` when nothing has registered that tag.
pub fn name_of(magic: u32) -> Option<&'static str> {
    all().iter().find(|(m, _)| *m == magic).map(|(_, n)| *n)
}

/// The registered tag whose display name is `name` — how a cached scan result is
/// restored to the same `&'static str` it was saved from.
pub fn by_name(name: &str) -> Option<&'static str> {
    all().iter().find(|(_, n)| *n == name).map(|(_, n)| *n)
}

pub(crate) fn registry_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("zdbview").join("magics"))
}

fn user_entries() -> Vec<(u32, String)> {
    registry_path()
        .as_deref()
        .map(parse_file)
        .unwrap_or_default()
}

pub(crate) fn parse_file(path: &Path) -> Vec<(u32, String)> {
    std::fs::read_to_string(path)
        .map(|s| parse(&s))
        .unwrap_or_default()
}

/// Parse the registry file. Anything unparseable is skipped rather than fatal —
/// a typo in one line must not cost the user the other lines, or the session.
pub(crate) fn parse(contents: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((tag, name)) = line.split_once('=') else {
            continue;
        };
        let (tag, name) = (tag.trim(), name.trim());
        if name.is_empty() {
            continue;
        }
        if let Some(m) = parse_tag(tag) {
            out.push((m, name.to_string()));
        }
    }
    out
}

/// A tag as either four printable ASCII characters or an explicit `0x…` u32.
pub(crate) fn parse_tag(tag: &str) -> Option<u32> {
    if let Some(hex) = tag.strip_prefix("0x").or_else(|| tag.strip_prefix("0X")) {
        return u32::from_str_radix(&hex.replace('_', ""), 16).ok();
    }
    let b = tag.as_bytes();
    (b.len() == 4 && b.iter().all(|c| c.is_ascii_graphic()))
        .then(|| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Builtins first, then user entries: a user name for a builtin tag replaces it,
/// a new tag is appended. Later duplicates within the file lose to earlier ones,
/// so the file reads top-down like every other config here.
fn merge(builtin: &[(u32, &'static str)], user: &[(u32, String)]) -> Vec<(u32, &'static str)> {
    let mut out: Vec<(u32, &'static str)> = builtin.to_vec();
    for (magic, name) in user {
        if out.iter().any(|(m, n)| m == magic && n == name) {
            continue;
        }
        let leaked: &'static str = Box::leak(name.clone().into_boxed_str());
        match out.iter_mut().find(|(m, _)| m == magic) {
            Some(slot) => slot.1 = leaked,
            None => out.push((*magic, leaked)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{merge, parse, parse_tag};

    #[test]
    fn a_tag_is_four_characters_or_explicit_hex() {
        // The bytes a producer stamping `u32::from_be_bytes(*b"ZRSC")` writes.
        assert_eq!(parse_tag("ZRSC"), Some(0x5A52_5343));
        assert_eq!(parse_tag("0x5A52_5343"), Some(0x5A52_5343));
        assert_eq!(parse_tag("ZRS"), None);
        assert_eq!(parse_tag("ZRSCX"), None);
        assert_eq!(parse_tag("ZR C"), None);
        assert_eq!(parse_tag("0xnothex"), None);
    }

    #[test]
    fn bad_lines_do_not_cost_the_good_ones() {
        let got = parse(
            "# a comment\n\nLUAR = luars cache (LUAR)\ngarbage\nNOPE =\n0x1234 = raw tag\n",
        );
        assert_eq!(
            got,
            vec![
                (0x4C55_4152, "luars cache (LUAR)".to_string()),
                (0x0000_1234, "raw tag".to_string()),
            ]
        );
    }

    #[test]
    fn a_user_entry_renames_a_builtin_and_adds_a_new_tag() {
        let builtin: &[(u32, &'static str)] = &[(1, "one"), (2, "two")];
        let user = vec![(2, "TWO, renamed".to_string()), (3, "three".to_string())];
        assert_eq!(
            merge(builtin, &user),
            vec![(1, "one"), (2, "TWO, renamed"), (3, "three")]
        );
    }
}
