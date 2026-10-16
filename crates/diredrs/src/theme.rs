//! Dired face colors, with light/dark-background variants.
//!
//! The faces mirror Emacs' `dired-directory`, `dired-symlink`,
//! `dired-executable`, `dired-marked`, `dired-flagged`, and `dired-header`.

use serde::Deserialize;

use crate::buffer::{Color, Style};

/// Which background the terminal uses, selecting the face variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Background {
    #[default]
    Dark,
    Light,
}

/// The set of faces used for rendering.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Regular files (the terminal default).
    pub file: Style,
    /// `dired-directory`: directory names.
    pub dir: Style,
    /// `dired-symlink`: symbolic links.
    pub symlink: Style,
    /// `dired-executable`: executable files.
    pub exec: Style,
    /// `dired-marked`: the `*` marker and marked names.
    pub marked: Style,
    /// `dired-flagged`: the `D` marker and flagged names.
    pub flagged: Style,
    /// The `  /dir:` and `total` header lines.
    pub header: Style,
    /// Background of the hex viewer's highlighted byte (ASCII and hex halves),
    /// matching Emacs's `highlight` face.
    pub hex_highlight: Color,
}

impl Theme {
    pub fn for_background(background: Background) -> Theme {
        match background {
            Background::Dark => Theme {
                file: Style::default(),
                dir: Style::rgb(0x87, 0xce, 0xfa).bold(), // LightSkyBlue
                symlink: Style::rgb(0x7f, 0xff, 0xd4).bold(), // Aquamarine
                exec: Style::rgb(0x90, 0xee, 0x90).bold(), // LightGreen
                marked: Style::rgb(0xff, 0x00, 0xff).bold(), // Magenta
                flagged: Style::rgb(0xff, 0x00, 0x00).bold(), // Red1
                header: Style::rgb(0xff, 0xff, 0x00).bold(), // Yellow
                hex_highlight: Color::Rgb(0x55, 0x6b, 0x2f), // DarkOliveGreen
            },
            Background::Light => Theme {
                file: Style::default(),
                dir: Style::rgb(0x00, 0x00, 0xff).bold(), // Blue1
                symlink: Style::rgb(0x00, 0x8b, 0x8b).bold(), // DarkCyan
                exec: Style::rgb(0x00, 0x8b, 0x00).bold(), // DarkGreen
                marked: Style::rgb(0x8b, 0x00, 0x8b).bold(), // Magenta4
                flagged: Style::rgb(0x8b, 0x00, 0x00).bold(), // DarkRed
                header: Style::rgb(0x8b, 0x8b, 0x00).bold(), // DarkYellow
                hex_highlight: Color::Rgb(0xb4, 0xee, 0xb4), // DarkSeaGreen2
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_and_light_differ() {
        let dark = Theme::for_background(Background::Dark);
        let light = Theme::for_background(Background::Light);
        assert_eq!(dark.dir, Style::rgb(0x87, 0xce, 0xfa).bold());
        assert_eq!(light.dir, Style::rgb(0x00, 0x00, 0xff).bold());
        assert!(dark.marked.bold);
        assert!(light.flagged.bold);
        // Hex byte highlight: Emacs `highlight` face (DarkOliveGreen /
        // DarkSeaGreen2).
        assert_eq!(dark.hex_highlight, Color::Rgb(0x55, 0x6b, 0x2f));
        assert_eq!(light.hex_highlight, Color::Rgb(0xb4, 0xee, 0xb4));
    }
}
