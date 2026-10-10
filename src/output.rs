use std::io::IsTerminal;

fn color_enabled() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").as_deref() != Ok("dumb")
}

fn paint(text: &str, code: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.into()
    }
}

/// Print the application heading, escaping controls in the subtitle.
pub fn heading(subtitle: &str) {
    println!(
        "\n  {}  {}",
        paint("WUMPA", "1;38;2;125;211;252"),
        paint(&clean(subtitle), "38;2;148;184;210")
    );
    println!(
        "  {}\n",
        paint(
            "────────────────────────────────────────────",
            "38;2;58;99;133"
        )
    );
}

/// Print a labeled value without allowing embedded terminal controls.
pub fn info(label: &str, value: impl std::fmt::Display) {
    println!(
        "  {}  {}",
        paint(&format!("{:<13}", clean(label)), "38;2;125;211;252"),
        clean(&value.to_string())
    );
}

/// Print a hint, escaping controls before applying optional color.
pub fn hint(text: &str) {
    println!("\n  {}\n", paint(&clean(text), "38;2;148;184;210"));
}

/// Print an error safely even when its message comes from a remote server.
pub fn error(message: impl std::fmt::Display) {
    eprintln!("wumpa: {}", clean(&message.to_string()));
}

/// Escape control characters while preserving ordinary Unicode text.
pub fn clean(value: &str) -> String {
    let mut result = String::new();
    for c in value.chars() {
        if c.is_control()
            || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            result.extend(c.escape_default());
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_terminal_controls_without_changing_unicode() {
        assert_eq!(clean("é\u{1b}[2J\n\r\u{7}"), "é\\u{1b}[2J\\n\\r\\u{7}");
        assert!(!clean("\u{9b}31m").chars().any(char::is_control));
        assert_eq!(
            clean("name\u{2028}\u{2029}\u{202e}\u{2066}"),
            "name\\u{2028}\\u{2029}\\u{202e}\\u{2066}"
        );
    }
}
