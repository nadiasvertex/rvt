//! The terminal's colors: a dark theme for the 16 ANSI colors, xterm's 256-color cube and gray
//! ramp, and the colors a program set for itself.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

const ANSI: [u32; 16] = [
    0x1d1f21, 0xcc6666, 0xb5bd68, 0xf0c674, 0x81a2be, 0xb294bb, 0x8abeb7, 0xc5c8c6, // normal
    0x666666, 0xd54e53, 0xb9ca4a, 0xe7c547, 0x7aa6da, 0xc397d8, 0x70c0b1, 0xeaeaea, // bright
];
pub const FOREGROUND: Rgb = hex(0xd8dadb);
pub const BACKGROUND: Rgb = hex(0x17181a);
pub const CURSOR: Rgb = hex(0xd8dadb);
pub const SELECTION: Rgb = hex(0x3a4250);

const fn hex(v: u32) -> Rgb {
    Rgb {
        r: (v >> 16) as u8,
        g: (v >> 8) as u8,
        b: v as u8,
    }
}

/// Color `i` of the 256-color palette, before a program changes it.
pub fn indexed(i: u8) -> Rgb {
    match i {
        0..=15 => hex(ANSI[i as usize]),
        16..=231 => {
            let i = i - 16;
            let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
            Rgb {
                r: level(i / 36),
                g: level(i / 6 % 6),
                b: level(i % 6),
            }
        }
        _ => {
            let v = 8 + 10 * (i - 232);
            Rgb { r: v, g: v, b: v }
        }
    }
}

fn dim(c: Rgb) -> Rgb {
    let f = |v: u8| (v as u16 * 2 / 3) as u8;
    Rgb {
        r: f(c.r),
        g: f(c.g),
        b: f(c.b),
    }
}

/// A named color, unless the program changed it, which `colors` records.
pub fn named(name: NamedColor, colors: &Colors) -> Rgb {
    if let Some(c) = colors[name] {
        return c;
    }
    let n = name as usize;
    match name {
        NamedColor::Foreground | NamedColor::BrightForeground => FOREGROUND,
        NamedColor::Background => BACKGROUND,
        NamedColor::Cursor => CURSOR,
        NamedColor::DimForeground => dim(FOREGROUND),
        _ if n < 16 => resolve_index(n as u8, colors),
        // The dim colors follow Cursor in order, from DimBlack.
        _ => dim(resolve_index(
            (n - NamedColor::DimBlack as usize) as u8,
            colors,
        )),
    }
}

fn resolve_index(i: u8, colors: &Colors) -> Rgb {
    colors[i as usize].unwrap_or_else(|| indexed(i))
}

/// The color of a cell's foreground or background.
pub fn resolve(color: Color, colors: &Colors) -> Rgb {
    match color {
        Color::Spec(rgb) => rgb,
        Color::Indexed(i) => resolve_index(i, colors),
        Color::Named(name) => named(name, colors),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cube_and_ramp_follow_xterm() {
        assert_eq!(indexed(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            indexed(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(indexed(196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(indexed(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            indexed(255),
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
    }

    #[test]
    fn a_color_set_by_the_program_wins() {
        let mut colors = Colors::default();
        assert_eq!(named(NamedColor::Red, &colors), indexed(1));
        colors[NamedColor::Red] = Some(hex(0x123456));
        assert_eq!(named(NamedColor::Red, &colors), hex(0x123456));
        assert_eq!(resolve(Color::Indexed(1), &colors), hex(0x123456));
        assert_eq!(named(NamedColor::DimRed, &colors), dim(hex(0x123456)));
    }
}
