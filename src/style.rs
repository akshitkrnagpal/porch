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
