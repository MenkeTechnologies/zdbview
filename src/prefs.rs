//! Persistent preferences (appdata), ported in spirit from iftoprs's prefs.
//!
//! iftoprs serializes a rich `Prefs` struct to TOML at `~/.iftoprs.conf`.
//! zdbview persists a small `key = value` file at
//! `$XDG_CONFIG_HOME/zdbview/prefs` (falling back to `~/.config/zdbview/prefs`)
//! with no serde/toml dependency — currently just the selected color scheme,
//! extensible with more lines.

use crate::theme::ThemeName;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct Prefs {
    pub theme: ThemeName,
    /// A user-edited 6-color palette overriding the named scheme, if any.
    pub custom: Option<[u8; 6]>,
}

fn prefs_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("zdbview").join("prefs"))
}

/// Load prefs; a missing or unparseable file yields defaults. Callers that
/// already know the active scheme should carry it rather than re-reading, so a
/// concurrent write cannot change what they are showing.
pub fn load() -> Prefs {
    match prefs_path() {
        Some(p) => load_from(&p),
        None => Prefs::default(),
    }
}

pub(crate) fn load_from(path: &std::path::Path) -> Prefs {
    let mut prefs = Prefs::default();
    if let Ok(contents) = std::fs::read_to_string(path) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                match k {
                    "theme" => {
                        if let Some(t) = ThemeName::from_token(v) {
                            prefs.theme = t;
                        }
                    }
                    "custom" => {
                        let nums: Vec<u8> =
                            v.split(',').filter_map(|n| n.trim().parse().ok()).collect();
                        if nums.len() == 6 {
                            prefs.custom =
                                Some([nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]]);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    prefs
}

/// Persist prefs (best-effort; creates the parent directory).
pub fn save(prefs: &Prefs) {
    if let Some(path) = prefs_path() {
        save_to(&path, prefs);
    }
}

pub(crate) fn save_to(path: &std::path::Path, prefs: &Prefs) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut body = format!("theme = {}\n", prefs.theme.token());
    if let Some(c) = prefs.custom {
        body.push_str(&format!(
            "custom = {},{},{},{},{},{}\n",
            c[0], c[1], c[2], c[3], c[4], c[5]
        ));
    }
    // Temp file plus rename, like the recent-files list and the scan cache: a
    // plain write truncates first, so another instance reading at that moment
    // sees an empty file, parses nothing, and silently falls back to the default
    // scheme. That is what "my colorscheme randomly reset" was.
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("zdbview_prefs_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.join("prefs")
    }

    #[test]
    fn scheme_and_custom_palette_round_trip() {
        let path = scratch("roundtrip");
        save_to(
            &path,
            &Prefs {
                theme: ThemeName::BladeRunner,
                custom: Some([1, 2, 3, 4, 5, 6]),
            },
        );
        let back = load_from(&path);
        assert_eq!(back.theme, ThemeName::BladeRunner);
        assert_eq!(back.custom, Some([1, 2, 3, 4, 5, 6]));

        // Saving without a custom palette drops the line rather than keeping a
        // stale one.
        save_to(
            &path,
            &Prefs {
                theme: ThemeName::RedSector,
                custom: None,
            },
        );
        let back = load_from(&path);
        assert_eq!(back.theme, ThemeName::RedSector);
        assert_eq!(back.custom, None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The write must be atomic: with a plain truncate-then-write, another
    /// instance reading at that instant sees an empty file, parses nothing, and
    /// falls back to the default scheme — which is what randomly reset the
    /// colorscheme. Check that no truncated state is left behind and that the
    /// temp file is cleaned up by the rename.
    #[test]
    fn save_leaves_no_partial_file_behind() {
        let path = scratch("atomic");
        save_to(
            &path,
            &Prefs {
                theme: ThemeName::Zaibatsu,
                custom: None,
            },
        );
        assert!(path.is_file());
        assert!(
            !path.with_extension("tmp").exists(),
            "the temp file must be renamed away, not left as a sibling"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "theme = zaibatsu\n", "content must be complete");

        // Rewriting in place keeps exactly one file and no leftovers.
        for name in [ThemeName::DeepNet, ThemeName::AcidRain] {
            save_to(
                &path,
                &Prefs {
                    theme: name,
                    custom: None,
                },
            );
            assert_eq!(load_from(&path).theme, name);
            assert!(!path.with_extension("tmp").exists());
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// An empty or half-written file is what the old race produced; loading one
    /// must fall back to defaults rather than erroring, and that fallback is
    /// exactly why the write is now atomic.
    #[test]
    fn a_truncated_file_reads_as_defaults() {
        let path = scratch("truncated");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(load_from(&path).theme, ThemeName::default());
        std::fs::write(&path, b"theme = red_se").unwrap();
        assert_eq!(
            load_from(&path).theme,
            ThemeName::default(),
            "a half-written token is not a scheme"
        );
        // Unknown keys, comments and blank lines are ignored.
        std::fs::write(&path, b"# c\n\nnope = 1\ntheme = deep_net\n").unwrap();
        assert_eq!(load_from(&path).theme, ThemeName::DeepNet);
        // A missing file is defaults too.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(load_from(&path).theme, ThemeName::default());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
