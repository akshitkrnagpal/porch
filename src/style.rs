//! Terminal colors for human-readable output.
//!
//! Color is used only when the stream is a terminal and `NO_COLOR` is unset
//! (see no-color.org); `FORCE_COLOR`/`CLICOLOR_FORCE` turn it on regardless.
//! Piped output, JSON and logs stay plain.
use std::io::IsTerminal;

#[derive(Clone, Copy)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Text from a repository file (package.json, porch.json), safe to print:
/// control characters, which could move the cursor or rewrite the screen, and
/// bidirectional overrides, which could reorder what is shown, become escapes.
pub fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control()
            || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

pub fn enabled(stream: Stream) -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty() && v != "0");
    if set("NO_COLOR") {
        return false;
    }
    if set("FORCE_COLOR") || set("CLICOLOR_FORCE") {
        return true;
    }
    if std::env::var_os("TERM").is_some_and(|t| t == "dumb") {
        return false;
    }
    match stream {
        Stream::Stdout => std::io::stdout().is_terminal(),
        Stream::Stderr => std::io::stderr().is_terminal(),
    }
}

fn paint(stream: Stream, code: &str, text: &str) -> String {
    if enabled(stream) {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

pub fn bold(s: Stream, t: &str) -> String {
    paint(s, "1", t)
}
pub fn dim(s: Stream, t: &str) -> String {
    paint(s, "2", t)
}
pub fn green(s: Stream, t: &str) -> String {
    paint(s, "32", t)
}
pub fn yellow(s: Stream, t: &str) -> String {
    paint(s, "33", t)
}
pub fn red(s: Stream, t: &str) -> String {
    paint(s, "31", t)
}
pub fn cyan(s: Stream, t: &str) -> String {
    paint(s, "36", t)
}
/// A bold, underlined, clickable (OSC 8) URL in terminals that support it.
pub fn url(s: Stream, u: &str) -> String {
    if enabled(s) {
        format!("\x1b]8;;{u}\x1b\\\x1b[1;36m{u}\x1b[0m\x1b]8;;\x1b\\")
    } else {
        u.to_owned()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn printable_escapes_terminal_controls() {
        assert_eq!(super::printable("vite --port 3000"), "vite --port 3000");
        assert_eq!(
            super::printable("dev\u{1b}]0;owned\u{7}\r\nx"),
            "dev\\u{1b}]0;owned\\u{7}\\u{d}\\u{a}x"
        );
        assert_eq!(super::printable("a\u{202e}b\u{9b}c"), "a\\u{202e}b\\u{9b}c");
        assert_eq!(super::printable("café"), "café");
    }
}
