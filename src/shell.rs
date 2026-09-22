//! Shell escaping utilities.

/// The command interpreter workmux hands its snippets to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellDialect {
    /// sh, bash, zsh, ... -- single-quote strings and `$(...)`.
    Posix,
    /// cmd.exe -- `&` chains commands and quoting rules differ.
    Cmd,
    /// PowerShell -- more capable than cmd but not POSIX either.
    PowerShell,
}

/// Classify a shell path or program name.
pub fn dialect_of(shell: &str) -> ShellDialect {
    let name = std::path::Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell)
        .to_ascii_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    match stem {
        "pwsh" | "powershell" => ShellDialect::PowerShell,
        "cmd" => ShellDialect::Cmd,
        "bash" | "zsh" | "sh" | "dash" | "ksh" | "ash" => ShellDialect::Posix,
        // Unknown shells are treated as POSIX, matching the historical default.
        _ => ShellDialect::Posix,
    }
}

/// Shell used for interactive panes when `$SHELL` is unset.
pub const fn default_interactive_shell() -> &'static str {
    if cfg!(windows) {
        "cmd.exe"
    } else {
        "/bin/bash"
    }
}

/// Default `hook_shell` config: the interpreter lifecycle hooks run through.
pub fn default_hook_argv() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd.exe".to_string(), "/C".to_string()]
    } else {
        vec!["bash".to_string(), "-c".to_string()]
    }
}

/// Program and arguments that run `script` as a shell snippet.
pub fn snippet_argv(script: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd.exe".to_string(), "/C".to_string(), script.to_string()]
    } else {
        vec!["sh".to_string(), "-c".to_string(), script.to_string()]
    }
}

/// Escape single quotes within a string for use inside a single-quoted shell argument.
///
/// The caller is responsible for wrapping the result in single quotes.
/// Example: `format!("'{}'", shell_escape(s))`
pub fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Quote a string for safe use as a shell argument.
///
/// Returns the string unchanged if it contains only safe characters
/// (alphanumeric, `-`, `_`, `.`, `/`). Otherwise wraps it in single quotes
/// with internal single quotes escaped. Empty strings return `''`.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/')
    {
        s.to_string()
    } else {
        format!("'{}'", shell_escape(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "hello");
        assert_eq!(shell_escape("foo bar"), "foo bar");
    }

    #[test]
    fn test_shell_escape_single_quotes() {
        assert_eq!(
            shell_escape("echo 'hello world'"),
            "echo '\\''hello world'\\''"
        );
    }

    #[test]
    fn test_shell_escape_preserves_special_chars() {
        assert_eq!(shell_escape("$HOME"), "$HOME");
        assert_eq!(shell_escape("$(cmd)"), "$(cmd)");
        assert_eq!(shell_escape("a & b"), "a & b");
    }

    #[test]
    fn test_shell_quote_safe_passthrough() {
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("/usr/bin/foo"), "/usr/bin/foo");
        assert_eq!(shell_quote("my-file_v2.txt"), "my-file_v2.txt");
    }

    #[test]
    fn test_shell_quote_wraps_unsafe() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote("a & b"), "'a & b'");
    }

    #[test]
    fn test_shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_shell_quote_empty_string() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn dialect_of_classifies_known_shells() {
        assert_eq!(dialect_of("/bin/bash"), ShellDialect::Posix);
        assert_eq!(dialect_of("/usr/bin/zsh"), ShellDialect::Posix);
        assert_eq!(
            dialect_of("C:\\Windows\\system32\\cmd.exe"),
            ShellDialect::Cmd
        );
        assert_eq!(dialect_of("cmd.exe"), ShellDialect::Cmd);
        assert_eq!(dialect_of("pwsh.exe"), ShellDialect::PowerShell);
        assert_eq!(dialect_of("powershell"), ShellDialect::PowerShell);
        // Unknown shells keep the POSIX default so existing configs behave.
        assert_eq!(dialect_of("nu"), ShellDialect::Posix);
    }

    #[test]
    fn snippet_argv_targets_the_platform_shell() {
        let argv = snippet_argv("echo hello");
        assert_eq!(argv.last().map(String::as_str), Some("echo hello"));
        assert_eq!(argv.len(), 3);
        if cfg!(windows) {
            assert_eq!(argv[..2], ["cmd.exe".to_string(), "/C".to_string()]);
        } else {
            assert_eq!(argv[..2], ["sh".to_string(), "-c".to_string()]);
        }
    }
}
