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

/// Load prefs; missing/unparseable file yields defaults.
pub fn load() -> Prefs {
    let mut prefs = Prefs::default();
    let path = match prefs_path() {
        Some(p) => p,
        None => return prefs,
    };
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
    let path = match prefs_path() {
        Some(p) => p,
        None => return,
    };
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
    let _ = std::fs::write(path, body);
}
