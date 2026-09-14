//! Translate ANSI SGR escape sequences in log lines into styled HTML spans.
//!
//! Container logs frequently carry terminal color codes (Rust's `tracing`
//! subscriber, Go's zerolog, etc.). Rendering them verbatim shows `␛[2m`
//! noise; stripping them loses the level coloring. This module walks a line,
//! keeps track of the current SGR state, and emits `<span>`s with classes the
//! stylesheet maps back to colors. Any escape sequence that is not SGR
//! (cursor movement, OSC titles, …) is dropped.

use maud::{html, Markup, PreEscaped};
use std::fmt::Write;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Color {
    /// Standard palette index 0..=15 (8 normal + 8 bright).
    Indexed(u8),
    /// Anything from the 256-color cube or a 24-bit truecolor sequence.
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Style {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    inverse: bool,
    fg: Option<Color>,
    bg: Option<Color>,
}

impl Style {
    fn is_plain(&self) -> bool {
        *self == Style::default()
    }

    /// Apply one SGR parameter list (the numbers between `ESC[` and `m`).
    fn apply_sgr(&mut self, params: &str) {
        let mut nums = params
            .split(';')
            .map(|p| if p.is_empty() { 0 } else { p.parse::<u16>().unwrap_or(0) })
            .peekable();

        if params.is_empty() {
            *self = Style::default();
            return;
        }

        while let Some(n) = nums.next() {
            match n {
                0 => *self = Style::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                7 => self.inverse = true,
                9 => self.strike = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                27 => self.inverse = false,
                29 => self.strike = false,
                30..=37 => self.fg = Some(Color::Indexed((n - 30) as u8)),
                39 => self.fg = None,
                40..=47 => self.bg = Some(Color::Indexed((n - 40) as u8)),
                49 => self.bg = None,
                90..=97 => self.fg = Some(Color::Indexed((n - 90 + 8) as u8)),
                100..=107 => self.bg = Some(Color::Indexed((n - 100 + 8) as u8)),
                38 | 48 => {
                    let color = match nums.next() {
                        Some(5) => nums.next().map(|i| xterm256(i.min(255) as u8)),
                        Some(2) => {
                            let r = nums.next();
                            let g = nums.next();
                            let b = nums.next();
                            match (r, g, b) {
                                (Some(r), Some(g), Some(b)) => Some(Color::Rgb(
                                    r.min(255) as u8,
                                    g.min(255) as u8,
                                    b.min(255) as u8,
                                )),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if n == 38 {
                        self.fg = color;
                    } else {
                        self.bg = color;
                    }
                }
                _ => {}
            }
        }
    }

    fn open_tag(&self, out: &mut String) {
        let mut classes: Vec<String> = Vec::new();
        let mut inline = String::new();

        if self.bold {
            classes.push("ansi-bold".into());
        }
        if self.dim {
            classes.push("ansi-dim".into());
        }
        if self.italic {
            classes.push("ansi-italic".into());
        }
        if self.underline {
            classes.push("ansi-underline".into());
        }
        if self.strike {
            classes.push("ansi-strike".into());
        }
        if self.inverse {
            classes.push("ansi-inverse".into());
        }
        match self.fg {
            Some(Color::Indexed(i)) => classes.push(format!("ansi-fg-{}", i)),
            Some(Color::Rgb(r, g, b)) => {
                let _ = write!(inline, "color:rgb({},{},{});", r, g, b);
            }
            None => {}
        }
        match self.bg {
            Some(Color::Indexed(i)) => classes.push(format!("ansi-bg-{}", i)),
            Some(Color::Rgb(r, g, b)) => {
                let _ = write!(inline, "background-color:rgb({},{},{});", r, g, b);
            }
            None => {}
        }

        out.push_str("<span");
        if !classes.is_empty() {
            out.push_str(" class=\"");
            out.push_str(&classes.join(" "));
            out.push('"');
        }
        if !inline.is_empty() {
            out.push_str(" style=\"");
            out.push_str(&inline);
            out.push('"');
        }
        out.push('>');
    }
}

/// Map a 256-color palette index to RGB. Indices below 16 stay in the
/// themeable indexed palette; the 6x6x6 cube and grayscale ramp become
/// concrete RGB values.
fn xterm256(i: u8) -> Color {
    match i {
        0..=15 => Color::Indexed(i),
        16..=231 => {
            let i = i - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color::Rgb(step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            Color::Rgb(v, v, v)
        }
    }
}

fn push_escaped(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// Render one line of log text, translating SGR sequences into spans and
/// dropping every other escape sequence.
pub fn ansi_to_html(line: &str) -> Markup {
    // Fast path: the overwhelmingly common case has no escapes at all.
    if !line.contains('\x1b') {
        return html! { (line) };
    }

    let mut out = String::with_capacity(line.len() + 64);
    let mut style = Style::default();
    let mut open = false;
    let mut text = String::new();

    let flush = |out: &mut String, text: &mut String, style: &Style, open: &mut bool| {
        if text.is_empty() {
            return;
        }
        if !style.is_plain() && !*open {
            style.open_tag(out);
            *open = true;
        }
        push_escaped(out, text);
        text.clear();
    };

    let mut chars = line.char_indices().peekable();
    while let Some((idx, c)) = chars.next() {
        if c != '\x1b' {
            text.push(c);
            continue;
        }

        // ESC seen: decide what kind of sequence follows.
        let rest = &line[idx + 1..];
        let mut consumed = 0usize;

        if let Some(after_bracket) = rest.strip_prefix('[') {
            // CSI: parameters (0x30–0x3F), intermediates (0x20–0x2F), final (0x40–0x7E).
            let mut params_end = 0;
            let bytes = after_bracket.as_bytes();
            while params_end < bytes.len() && (0x20..=0x3f).contains(&bytes[params_end]) {
                params_end += 1;
            }
            if params_end < bytes.len() && (0x40..=0x7e).contains(&bytes[params_end]) {
                let final_byte = bytes[params_end];
                consumed = 1 + params_end + 1;
                if final_byte == b'm' {
                    let params = &after_bracket[..params_end];
                    flush(&mut out, &mut text, &style, &mut open);
                    if open {
                        out.push_str("</span>");
                        open = false;
                    }
                    style.apply_sgr(params);
                }
                // Non-SGR CSI sequences are silently dropped.
            } else {
                // Truncated sequence: drop the rest of the line's escape.
                consumed = 1 + params_end;
            }
        } else if let Some(after) = rest.strip_prefix(']') {
            // OSC: terminated by BEL or ESC \ (string terminator).
            let end = after
                .find('\x07')
                .map(|p| p + 1)
                .or_else(|| after.find("\x1b\\").map(|p| p + 2))
                .unwrap_or(after.len());
            consumed = 1 + end;
        } else if let Some(next) = rest.chars().next() {
            // Two-character escape (e.g. ESC ( B, ESC 7). Drop it.
            consumed = next.len_utf8();
            if matches!(next, '(' | ')' | '#' | '%') {
                if let Some(n2) = rest[consumed..].chars().next() {
                    consumed += n2.len_utf8();
                }
            }
        }

        // Advance the iterator past the consumed bytes.
        let target = idx + 1 + consumed;
        while let Some(&(i, _)) = chars.peek() {
            if i >= target {
                break;
            }
            chars.next();
        }
    }

    flush(&mut out, &mut text, &style, &mut open);
    if open {
        out.push_str("</span>");
    }

    PreEscaped(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(s: &str) -> String {
        ansi_to_html(s).into_string()
    }

    #[test]
    fn plain_text_is_escaped_unchanged() {
        assert_eq!(render("a < b & c"), "a &lt; b &amp; c");
    }

    #[test]
    fn tracing_style_line() {
        let line = "\x1b[2m2026-09-13T22:01:54Z\x1b[0m \x1b[32m INFO\x1b[0m \x1b[2mapp::sync\x1b[0m\x1b[2m:\x1b[0m board fetched \x1b[3mrows\x1b[0m\x1b[2m=\x1b[0m6";
        assert_eq!(
            render(line),
            "<span class=\"ansi-dim\">2026-09-13T22:01:54Z</span> \
             <span class=\"ansi-fg-2\"> INFO</span> \
             <span class=\"ansi-dim\">app::sync</span><span class=\"ansi-dim\">:</span> board fetched \
             <span class=\"ansi-italic\">rows</span><span class=\"ansi-dim\">=</span>6"
        );
    }

    #[test]
    fn combined_attributes_and_bright_colors() {
        assert_eq!(
            render("\x1b[1;91mERR\x1b[0m"),
            "<span class=\"ansi-bold ansi-fg-9\">ERR</span>"
        );
    }

    #[test]
    fn truecolor_and_256() {
        assert_eq!(
            render("\x1b[38;2;10;20;30mx\x1b[0m"),
            "<span style=\"color:rgb(10,20,30);\">x</span>"
        );
        assert_eq!(
            render("\x1b[38;5;196mx\x1b[0m"),
            "<span style=\"color:rgb(255,0,0);\">x</span>"
        );
        assert_eq!(
            render("\x1b[48;5;4mx\x1b[0m"),
            "<span class=\"ansi-bg-4\">x</span>"
        );
    }

    #[test]
    fn partial_resets() {
        assert_eq!(
            render("\x1b[1;31mA\x1b[22mB\x1b[39mC"),
            "<span class=\"ansi-bold ansi-fg-1\">A</span><span class=\"ansi-fg-1\">B</span>C"
        );
    }

    #[test]
    fn non_sgr_sequences_are_dropped() {
        assert_eq!(render("a\x1b[2Kb\x1b[1;1Hc"), "abc");
        assert_eq!(render("a\x1b]0;title\x07b"), "ab");
        assert_eq!(render("a\x1b]0;title\x1b\\b"), "ab");
        assert_eq!(render("a\x1b(Bb"), "ab");
    }

    #[test]
    fn truncated_escape_does_not_panic() {
        assert_eq!(render("abc\x1b["), "abc");
        assert_eq!(render("abc\x1b"), "abc");
        assert_eq!(render("abc\x1b[31"), "abc");
    }

    #[test]
    fn html_inside_styled_span_is_escaped() {
        assert_eq!(
            render("\x1b[31m<b>\x1b[0m"),
            "<span class=\"ansi-fg-1\">&lt;b&gt;</span>"
        );
    }
}
