//! Face colors, with light/dark-background variants.
//!
//! `dired-*` faces mirror Emacs' Dired, and [`SyntaxFaces`] mirrors the
//! `font-lock-*` / markdown-mode faces used for syntax highlighting. The
//! colours are Emacs' defaults (from `font-lock.el`, `faces.el`, and
//! markdown-mode.el), resolved to hex via `colors_list.txt`.

use serde::Deserialize;

use crate::buffer::{Color, Style};

/// Faces used for syntax highlighting, matching Emacs' defaults for the
/// 88-colour (`min-colors 88`) class on dark or light backgrounds.
#[derive(Debug, Clone, Copy)]
pub struct SyntaxFaces {
    /// `font-lock-comment-face`.
    pub comment: Style,
    /// `font-lock-string-face`.
    pub string: Style,
    /// `font-lock-keyword-face`.
    pub keyword: Style,
    /// `font-lock-builtin-face`.
    pub builtin: Style,
    /// `font-lock-function-name-face` (also `markdown-header-face`).
    pub function: Style,
    /// `font-lock-variable-name-face`.
    pub variable: Style,
    /// `font-lock-type-face`.
    pub r#type: Style,
    /// `font-lock-constant-face`.
    pub constant: Style,
    /// `shadow` (markdown markup/delimiters, list markers).
    pub markup: Style,
    /// `link` (markdown link text).
    pub link: Style,
}

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
    /// Faces for syntax highlighting.
    pub syntax: SyntaxFaces,
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
                syntax: dark_syntax(),
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
                syntax: light_syntax(),
            },
        }
    }
}

/// `font-lock-*` face colours on a dark background.
fn dark_syntax() -> SyntaxFaces {
    SyntaxFaces {
        comment: Style::rgb(0xff, 0x7f, 0x24),          // chocolate1
        string: Style::rgb(0xff, 0xa0, 0x7a),           // LightSalmon
        keyword: Style::rgb(0x00, 0xff, 0xff),          // Cyan1
        builtin: Style::rgb(0xb0, 0xc4, 0xde),          // LightSteelBlue
        function: Style::rgb(0x87, 0xce, 0xfa),         // LightSkyBlue
        variable: Style::rgb(0xee, 0xdd, 0x82),         // LightGoldenrod
        r#type: Style::rgb(0x98, 0xfb, 0x98),           // PaleGreen
        constant: Style::rgb(0x7f, 0xff, 0xd4),         // Aquamarine
        markup: Style::rgb(0xb3, 0xb3, 0xb3),           // grey70
        link: Style::rgb(0x00, 0xff, 0xff).underline(), // Cyan1
    }
}

/// `font-lock-*` face colours on a light background.
fn light_syntax() -> SyntaxFaces {
    SyntaxFaces {
        comment: Style::rgb(0xb2, 0x22, 0x22),          // Firebrick
        string: Style::rgb(0x8b, 0x22, 0x52),           // VioletRed4
        keyword: Style::rgb(0xa0, 0x20, 0xf0),          // Purple
        builtin: Style::rgb(0x48, 0x3d, 0x8b),          // DarkSlateBlue
        function: Style::rgb(0x00, 0x00, 0xff),         // Blue1
        variable: Style::rgb(0xa0, 0x52, 0x2d),         // Sienna
        r#type: Style::rgb(0x22, 0x8b, 0x22),           // ForestGreen
        constant: Style::rgb(0x00, 0x8b, 0x8b),         // DarkCyan
        markup: Style::rgb(0x7f, 0x7f, 0x7f),           // grey50
        link: Style::rgb(0x3a, 0x5f, 0xcd).underline(), // RoyalBlue3
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
        // Syntax faces: font-lock-* defaults (chocolate1 / Firebrick comments).
        assert_eq!(dark.syntax.comment, Style::rgb(0xff, 0x7f, 0x24));
        assert_eq!(light.syntax.comment, Style::rgb(0xb2, 0x22, 0x22));
        assert!(dark.syntax.link.underline);
    }
}
