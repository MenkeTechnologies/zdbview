//! Color schemes, ported from iftoprs.
//!
//! Each of the 31 named schemes is a 6-color ANSI-256 palette. iftoprs's
//! `Theme` mapped those onto its network-monitor UI (bars, hosts, rates); here
//! the same 6 base colors are mapped onto zdbview's UI roles (primary, accent,
//! alt, label, dim) plus the overlay roles the help / chooser / editor modals
//! use. The palettes themselves are copied verbatim so the schemes look
//! identical across the two tools.

use ratatui::style::Color;

/// The 31 named color schemes (ported verbatim from iftoprs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeName {
    #[default]
    NeonSprawl,
    AcidRain,
    IceBreaker,
    SynthWave,
    RustBelt,
    GhostWire,
    RedSector,
    SakuraDen,
    DataStream,
    SolarFlare,
    NeonNoir,
    ChromeHeart,
    BladeRunner,
    VoidWalker,
    ToxicWaste,
    CyberFrost,
    PlasmaCore,
    SteelNerve,
    DarkSignal,
    GlitchPop,
    HoloShift,
    NightCity,
    DeepNet,
    LaserGrid,
    QuantumFlux,
    BioHazard,
    Darkwave,
    Overlock,
    Megacorp,
    Zaibatsu,
    Iftopcolor,
}

impl ThemeName {
    /// All schemes, in chooser order.
    pub const ALL: &'static [ThemeName] = &[
        ThemeName::NeonSprawl,
        ThemeName::AcidRain,
        ThemeName::IceBreaker,
        ThemeName::SynthWave,
        ThemeName::RustBelt,
        ThemeName::GhostWire,
        ThemeName::RedSector,
        ThemeName::SakuraDen,
        ThemeName::DataStream,
        ThemeName::SolarFlare,
        ThemeName::NeonNoir,
        ThemeName::ChromeHeart,
        ThemeName::BladeRunner,
        ThemeName::VoidWalker,
        ThemeName::ToxicWaste,
        ThemeName::CyberFrost,
        ThemeName::PlasmaCore,
        ThemeName::SteelNerve,
        ThemeName::DarkSignal,
        ThemeName::GlitchPop,
        ThemeName::HoloShift,
        ThemeName::NightCity,
        ThemeName::DeepNet,
        ThemeName::LaserGrid,
        ThemeName::QuantumFlux,
        ThemeName::BioHazard,
        ThemeName::Darkwave,
        ThemeName::Overlock,
        ThemeName::Megacorp,
        ThemeName::Zaibatsu,
        ThemeName::Iftopcolor,
    ];

    /// Human-readable name.
    pub fn display(self) -> &'static str {
        match self {
            ThemeName::NeonSprawl => "Neon Sprawl",
            ThemeName::AcidRain => "Acid Rain",
            ThemeName::IceBreaker => "Ice Breaker",
            ThemeName::SynthWave => "Synth Wave",
            ThemeName::RustBelt => "Rust Belt",
            ThemeName::GhostWire => "Ghost Wire",
            ThemeName::RedSector => "Red Sector",
            ThemeName::SakuraDen => "Sakura Den",
            ThemeName::DataStream => "Data Stream",
            ThemeName::SolarFlare => "Solar Flare",
            ThemeName::NeonNoir => "Neon Noir",
            ThemeName::ChromeHeart => "Chrome Heart",
            ThemeName::BladeRunner => "Blade Runner",
            ThemeName::VoidWalker => "Void Walker",
            ThemeName::ToxicWaste => "Toxic Waste",
            ThemeName::CyberFrost => "Cyber Frost",
            ThemeName::PlasmaCore => "Plasma Core",
            ThemeName::SteelNerve => "Steel Nerve",
            ThemeName::DarkSignal => "Dark Signal",
            ThemeName::GlitchPop => "Glitch Pop",
            ThemeName::HoloShift => "Holo Shift",
            ThemeName::NightCity => "Night City",
            ThemeName::DeepNet => "Deep Net",
            ThemeName::LaserGrid => "Laser Grid",
            ThemeName::QuantumFlux => "Quantum Flux",
            ThemeName::BioHazard => "Bio Hazard",
            ThemeName::Darkwave => "Darkwave",
            ThemeName::Overlock => "Overlock",
            ThemeName::Megacorp => "Megacorp",
            ThemeName::Zaibatsu => "Zaibatsu",
            ThemeName::Iftopcolor => "Iftop Color",
        }
    }

    /// Stable token used for prefs persistence.
    pub fn token(self) -> &'static str {
        match self {
            ThemeName::NeonSprawl => "neon_sprawl",
            ThemeName::AcidRain => "acid_rain",
            ThemeName::IceBreaker => "ice_breaker",
            ThemeName::SynthWave => "synth_wave",
            ThemeName::RustBelt => "rust_belt",
            ThemeName::GhostWire => "ghost_wire",
            ThemeName::RedSector => "red_sector",
            ThemeName::SakuraDen => "sakura_den",
            ThemeName::DataStream => "data_stream",
            ThemeName::SolarFlare => "solar_flare",
            ThemeName::NeonNoir => "neon_noir",
            ThemeName::ChromeHeart => "chrome_heart",
            ThemeName::BladeRunner => "blade_runner",
            ThemeName::VoidWalker => "void_walker",
            ThemeName::ToxicWaste => "toxic_waste",
            ThemeName::CyberFrost => "cyber_frost",
            ThemeName::PlasmaCore => "plasma_core",
            ThemeName::SteelNerve => "steel_nerve",
            ThemeName::DarkSignal => "dark_signal",
            ThemeName::GlitchPop => "glitch_pop",
            ThemeName::HoloShift => "holo_shift",
            ThemeName::NightCity => "night_city",
            ThemeName::DeepNet => "deep_net",
            ThemeName::LaserGrid => "laser_grid",
            ThemeName::QuantumFlux => "quantum_flux",
            ThemeName::BioHazard => "bio_hazard",
            ThemeName::Darkwave => "darkwave",
            ThemeName::Overlock => "overlock",
            ThemeName::Megacorp => "megacorp",
            ThemeName::Zaibatsu => "zaibatsu",
            ThemeName::Iftopcolor => "iftop_color",
        }
    }

    pub fn from_token(s: &str) -> Option<ThemeName> {
        Self::ALL.iter().copied().find(|t| t.token() == s)
    }
}

/// The 6-color base palette for a scheme (ANSI-256 indices). Copied verbatim
/// from iftoprs so schemes render identically.
fn palette(name: ThemeName) -> (u8, u8, u8, u8, u8, u8) {
    match name {
        ThemeName::NeonSprawl => (27, 48, 135, 141, 63, 99),
        ThemeName::AcidRain => (28, 46, 34, 40, 22, 35),
        ThemeName::IceBreaker => (19, 39, 25, 33, 21, 32),
        ThemeName::SynthWave => (91, 177, 128, 134, 93, 97),
        ThemeName::RustBelt => (172, 214, 178, 220, 166, 130),
        ThemeName::GhostWire => (37, 50, 44, 87, 30, 23),
        ThemeName::RedSector => (160, 203, 196, 210, 124, 88),
        ThemeName::SakuraDen => (175, 218, 182, 225, 169, 132),
        ThemeName::DataStream => (22, 46, 28, 119, 34, 22),
        ThemeName::SolarFlare => (202, 220, 196, 213, 160, 125),
        ThemeName::NeonNoir => (201, 231, 93, 219, 57, 53),
        ThemeName::ChromeHeart => (250, 255, 246, 253, 243, 239),
        ThemeName::BladeRunner => (208, 37, 166, 73, 130, 23),
        ThemeName::VoidWalker => (55, 99, 54, 141, 92, 17),
        ThemeName::ToxicWaste => (118, 190, 154, 226, 82, 58),
        ThemeName::CyberFrost => (159, 195, 153, 189, 111, 67),
        ThemeName::PlasmaCore => (199, 213, 163, 207, 126, 89),
        ThemeName::SteelNerve => (68, 110, 60, 146, 24, 236),
        ThemeName::DarkSignal => (30, 43, 23, 79, 29, 16),
        ThemeName::GlitchPop => (201, 51, 226, 47, 196, 21),
        ThemeName::HoloShift => (123, 219, 159, 183, 87, 133),
        ThemeName::NightCity => (214, 227, 209, 223, 172, 94),
        ThemeName::DeepNet => (19, 33, 17, 75, 26, 16),
        ThemeName::LaserGrid => (46, 201, 51, 226, 196, 21),
        ThemeName::QuantumFlux => (135, 75, 171, 111, 98, 61),
        ThemeName::BioHazard => (148, 184, 106, 192, 64, 22),
        ThemeName::Darkwave => (53, 140, 89, 176, 127, 52),
        ThemeName::Overlock => (196, 208, 160, 214, 124, 52),
        ThemeName::Megacorp => (252, 39, 245, 81, 242, 236),
        ThemeName::Zaibatsu => (167, 216, 131, 224, 95, 52),
        ThemeName::Iftopcolor => (21, 46, 28, 48, 33, 19),
    }
}

/// zdbview's active color set, derived from a scheme's 6 base colors. The role
/// mapping mirrors iftoprs's (c1 primary, c2 bright accent, c3 alt, c4 mid,
/// c5 dim; c6 becomes the overlay section color).
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: ThemeName,
    /// Bright accent — borders, focus, titles, the input cursor.
    pub accent: Color,
    /// Primary — headers, keys, emphasis, warnings.
    pub primary: Color,
    /// Alternate — schema object types, the rkyv format badge.
    pub alt: Color,
    /// Mid — section labels, field names, format name.
    pub label: Color,
    /// Dim — secondary text and separators.
    pub dim: Color,
    /// Overlay fill — the modal background behind help / chooser / editor.
    pub help_bg: Color,
    /// Overlay frame.
    pub help_border: Color,
    /// Overlay title line.
    pub help_title: Color,
    /// Overlay section headings.
    pub help_section: Color,
    /// Overlay key names (also the chooser's selected-row background).
    pub help_key: Color,
    /// Overlay values / footer text.
    pub help_val: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::from_name(ThemeName::default())
    }
}

impl Theme {
    pub fn from_name(name: ThemeName) -> Self {
        let (c1, c2, c3, c4, c5, c6) = palette(name);
        Self::from_palette(name, [c1, c2, c3, c4, c5, c6])
    }

    /// Build a theme from an explicit 6-color palette (used by the editor).
    ///
    /// The overlay roles use the same derivation as iftoprs's
    /// `Theme::from_palette_raw`: a fixed dark fill (236) with the frame and
    /// title on the primary, sections on the darkest base color, keys on the
    /// accent, and values on the mid color.
    pub fn from_palette(name: ThemeName, p: [u8; 6]) -> Self {
        Theme {
            name,
            primary: Color::Indexed(p[0]),
            accent: Color::Indexed(p[1]),
            alt: Color::Indexed(p[2]),
            label: Color::Indexed(p[3]),
            dim: Color::Indexed(p[4]),
            help_bg: Color::Indexed(236),
            help_border: Color::Indexed(p[0]),
            help_title: Color::Indexed(p[0]),
            help_section: Color::Indexed(p[5]),
            help_key: Color::Indexed(p[1]),
            help_val: Color::Indexed(p[3]),
        }
    }

    /// A 6-cell color swatch for the scheme, used by the chooser and
    /// `--list-themes` (ported from iftoprs's `Theme::swatch`).
    pub fn swatch(name: ThemeName) -> [(Color, &'static str); 6] {
        let p = base_palette(name);
        [
            (Color::Indexed(p[0]), "██"),
            (Color::Indexed(p[1]), "██"),
            (Color::Indexed(p[2]), "██"),
            (Color::Indexed(p[3]), "██"),
            (Color::Indexed(p[4]), "██"),
            (Color::Indexed(p[5]), "██"),
        ]
    }
}

/// The raw 6-color base palette for a scheme (for the editor to seed from).
pub fn base_palette(name: ThemeName) -> [u8; 6] {
    let (c1, c2, c3, c4, c5, c6) = palette(name);
    [c1, c2, c3, c4, c5, c6]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The palette table is copied from iftoprs; spot-check two schemes against
    /// `iftoprs/src/config/theme.rs` so a typo in either file is caught.
    #[test]
    fn palettes_match_iftoprs() {
        assert_eq!(
            base_palette(ThemeName::NeonSprawl),
            [27, 48, 135, 141, 63, 99]
        );
        assert_eq!(
            base_palette(ThemeName::BladeRunner),
            [208, 37, 166, 73, 130, 23]
        );
    }

    /// Every scheme must resolve all six overlay roles to indexed colors —
    /// a `Color::Reset` here would make a modal unreadable on that scheme.
    #[test]
    fn every_scheme_resolves_overlay_roles() {
        for &name in ThemeName::ALL {
            let t = Theme::from_name(name);
            for (role, c) in [
                ("help_bg", t.help_bg),
                ("help_border", t.help_border),
                ("help_title", t.help_title),
                ("help_section", t.help_section),
                ("help_key", t.help_key),
                ("help_val", t.help_val),
            ] {
                assert!(
                    matches!(c, Color::Indexed(_)),
                    "{}: {role} is not indexed",
                    name.token()
                );
            }
        }
    }

    /// The overlay roles are derived from the *edited* palette, not the named
    /// scheme's, so the editor previews custom colors live.
    #[test]
    fn overlay_roles_follow_edited_palette() {
        let t = Theme::from_palette(ThemeName::NeonSprawl, [1, 2, 3, 4, 5, 6]);
        assert_eq!(t.help_border, Color::Indexed(1));
        assert_eq!(t.help_title, Color::Indexed(1));
        assert_eq!(t.help_key, Color::Indexed(2));
        assert_eq!(t.help_val, Color::Indexed(4));
        assert_eq!(t.help_section, Color::Indexed(6));
        // Fill stays a fixed dark gray regardless of palette (matches iftoprs).
        assert_eq!(t.help_bg, Color::Indexed(236));
    }

    /// The chooser draws `swatch` next to the scheme name; its cells must be in
    /// base-palette order or the preview lies about the colors.
    #[test]
    fn swatch_cells_are_base_palette_in_order() {
        for &name in ThemeName::ALL {
            let p = base_palette(name);
            for (i, (color, block)) in Theme::swatch(name).iter().enumerate() {
                assert_eq!(*color, Color::Indexed(p[i]), "{}", name.token());
                assert_eq!(*block, "██");
            }
        }
    }

    /// Prefs persist the token, so tokens must be unique and roundtrip.
    #[test]
    fn tokens_are_unique_and_roundtrip() {
        let mut seen = std::collections::HashSet::new();
        for &name in ThemeName::ALL {
            assert!(
                seen.insert(name.token()),
                "duplicate token {}",
                name.token()
            );
            assert_eq!(ThemeName::from_token(name.token()), Some(name));
        }
        assert_eq!(ThemeName::from_token("no_such_scheme"), None);
    }

    /// `--list-themes` and the chooser both label schemes by `display`.
    #[test]
    fn display_names_are_unique_and_non_empty() {
        let mut seen = std::collections::HashSet::new();
        for &name in ThemeName::ALL {
            assert!(!name.display().is_empty());
            assert!(
                seen.insert(name.display()),
                "duplicate name {}",
                name.display()
            );
        }
    }

    /// A scheme that is not in `ALL` cannot be chosen, previewed or saved — the
    /// chooser, `--list-themes` and the prefs round trip all walk that list. The
    /// match is exhaustive on purpose: adding a scheme breaks this test's build,
    /// which is the reminder to list it.
    #[test]
    fn every_scheme_is_in_the_list_the_ui_walks() {
        match ThemeName::default() {
            ThemeName::NeonSprawl
            | ThemeName::AcidRain
            | ThemeName::IceBreaker
            | ThemeName::SynthWave
            | ThemeName::RustBelt
            | ThemeName::GhostWire
            | ThemeName::RedSector
            | ThemeName::SakuraDen
            | ThemeName::DataStream
            | ThemeName::SolarFlare
            | ThemeName::NeonNoir
            | ThemeName::ChromeHeart
            | ThemeName::BladeRunner
            | ThemeName::VoidWalker
            | ThemeName::ToxicWaste
            | ThemeName::CyberFrost
            | ThemeName::PlasmaCore
            | ThemeName::SteelNerve
            | ThemeName::DarkSignal
            | ThemeName::GlitchPop
            | ThemeName::HoloShift
            | ThemeName::NightCity
            | ThemeName::DeepNet
            | ThemeName::LaserGrid
            | ThemeName::QuantumFlux
            | ThemeName::BioHazard
            | ThemeName::Darkwave
            | ThemeName::Overlock
            | ThemeName::Megacorp
            | ThemeName::Zaibatsu
            | ThemeName::Iftopcolor => {}
        }
        assert_eq!(
            ThemeName::ALL.len(),
            31,
            "every scheme named above is listed, and nothing else is"
        );
        let mut seen: Vec<ThemeName> = ThemeName::ALL.to_vec();
        let before = seen.len();
        seen.sort_by_key(|t| t.token());
        seen.dedup();
        assert_eq!(before, seen.len(), "and none of them is listed twice");
    }
}
