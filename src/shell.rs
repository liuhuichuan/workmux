//! Shell escaping utilities.

use std::process::Command;

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

/// The name a shell answers to: its path's last component, without case and
/// without the suffix a Windows install gives an executable.
///
/// A shell is written down as a path on one host and as a bare name on another,
/// and Windows spells the same program `bash.EXE` where Unix spells it `bash`.
/// Every table that asks what a shell can do is keyed by this name, so that a
/// shell is recognised wherever its path came from -- a path carries the
/// separator of the platform that wrote it, not of the one reading it, so the
/// last component is taken off both.
pub fn shell_name(shell: &str) -> String {
    let name = shell
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or(shell)
        .to_ascii_lowercase();
    match name.strip_suffix(".exe") {
        Some(stem) => stem.to_string(),
        None => name,
    }
}

/// Classify a shell path or program name.
pub fn dialect_of(shell: &str) -> ShellDialect {
    match shell_name(shell).as_str() {
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

/// The words that start `shell` as an interactive pane's shell.
///
/// A shell that reads a profile is started as a login shell, which is what the
/// POSIX pane handshakes have always done (`exec '<shell>' -l`): the profile is
/// where a pane's PATH, aliases and environment come from, so a pane started
/// without it runs a shell the user would not recognise as their own. `cmd.exe`
/// and PowerShell have no such flag and are started as they are.
///
/// Only the Windows pane handshake starts a shell this way.
#[cfg(windows)]
pub fn interactive_shell_argv(shell: &str) -> Vec<String> {
    let mut argv = vec![shell.to_string()];
    if dialect_of(shell) == ShellDialect::Posix {
        argv.push("-l".to_string());
    }
    argv
}

/// Suffix that discards a command's output in the interpreter that runs
/// deferred scripts, which is `sh` on Unix and PowerShell on Windows (see
/// `multiplexer::util::deferred_script_command`).
///
/// The null device is part of the dialect: PowerShell parsed `>/dev/null` as a
/// file named `null` under a `\dev` directory and failed the statement.
pub const fn silent_output_suffix() -> &'static str {
    if cfg!(windows) {
        "> $null 2>&1"
    } else {
        ">/dev/null 2>&1"
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

/// Append `script` to `command` the way `shell` expects to receive it.
///
/// `cmd.exe` re-parses its own command line, so its snippet has to arrive
/// verbatim: the C runtime quoting `Command::arg` applies would escape the
/// quotes in e.g. `echo hi > "C:\dir\file"` and cmd would treat the backslashes
/// as part of the file name.
///
/// Verbatim is not enough on its own, because cmd.exe also reads the line
/// after `/C` with a quoting rule of its own: a line that opens with a quote
/// and carries more than two of them loses its first and its last, so
/// `"C:\Program Files\tool.exe" one two` arrives as
/// `C:\Program Files\tool.exe" one two` and cmd answers "The filename,
/// directory name, or volume label syntax is incorrect." One pair of quotes
/// around the whole snippet is the form cmd strips back to what was written:
/// it is the first and last character, so the snippet's own words survive.
/// Every other interpreter takes its snippet through standard argument
/// quoting.
pub fn append_snippet(command: &mut Command, shell: &str, script: &str) {
    #[cfg(not(windows))]
    let _ = shell;
    #[cfg(windows)]
    if dialect_of(shell) == ShellDialect::Cmd {
        use std::os::windows::process::CommandExt;
        command.raw_arg(format!("\"{script}\""));
        return;
    }
    command.arg(script);
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

/// Quote `value` as one argument for the shell `snippet_argv` names.
///
/// The shell that names is this platform's, so the quoting is `cmd_quote`'s on
/// Windows and `shell_quote`'s everywhere else.
///
/// A POSIX quoted argument is not a quoted argument to `cmd.exe`, which has no
/// single-quote form: it looks for a program whose name carries the quotes and
/// fails. cmd quotes with `"`, so anything outside the plain set is wrapped in
/// one. A path stays bare, which is also what lets it survive a hand-off that
/// escapes any quote it finds.
pub fn snippet_quote(value: &str) -> String {
    if cfg!(windows) {
        cmd_quote(value)
    } else {
        shell_quote(value)
    }
}

/// Quote `value` the way `cmd.exe` reads it.
///
/// cmd has no single-quote form, so anything outside the characters it reads as
/// text is wrapped in a double quote, and a path stays bare.
fn cmd_quote(value: &str) -> String {
    /// Characters cmd reads as syntax rather than as text.
    const UNSAFE: &[char] = &[' ', '\t', '"', '&', '|', '<', '>', '^', '(', ')', '%', '!'];

    if value.is_empty() {
        return "\"\"".to_string();
    }
    if !value.contains(UNSAFE) {
        return value.to_string();
    }
    // cmd has no escape for a quote inside a quoted argument, so doubling
    // it is as far as a command line can carry.
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Quote `value` as one word of a command line that `dialect` will read.
///
/// A word carrying nothing but characters every shell passes through is left
/// alone, which is what a Windows path needs: quoting a path the C runtime's
/// way hands PowerShell a string where it wants a program, and a quoted program
/// is only a program to PowerShell when the call operator names it.
///
/// `is_program` says whether the word is the command being run rather than one
/// of its arguments -- the two are quoted the same way everywhere but in
/// PowerShell, where the call operator is part of naming the program.
///
/// The dialect decides, not the machine: a word spelled for `cmd.exe` is
/// spelled that way wherever workmux runs, because the pane that reads it is
/// the one that named the dialect.
pub fn word_quote(value: &str, dialect: ShellDialect, is_program: bool) -> String {
    if !value.is_empty() && value.chars().all(is_word_character) {
        return value.to_string();
    }
    match dialect {
        ShellDialect::PowerShell => {
            let quoted = format!("'{}'", value.replace('\'', "''"));
            if is_program {
                format!("& {quoted}")
            } else {
                quoted
            }
        }
        ShellDialect::Cmd => cmd_quote(value),
        ShellDialect::Posix => shell_quote(value),
    }
}

/// Characters no shell here reads as syntax, so a word made of them needs no
/// quoting: the path separators of either platform, and the punctuation of
/// flags, versions and model names.
fn is_word_character(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '\\' | ':' | '+' | '~')
}

/// Split a command line into the words this platform's shells pass on.
///
/// The POSIX rules read a backslash as an escape, which is not what a Windows
/// path is: `C:\Users\me\claude --flag` comes back as `C:Usersmeclaude` and
/// `--flag`, and an agent configured by path is then a program nothing has
/// heard of. Windows keeps the backslash as a character -- it escapes a quote
/// and nothing else -- and groups words with quotes, so a command line read off
/// a config file is split the way the platform that will run it reads it.
pub fn split_command_line(command: &str) -> Option<Vec<String>> {
    #[cfg(not(windows))]
    {
        shlex::split(command)
    }
    #[cfg(windows)]
    {
        split_windows_command_line(command)
    }
}

/// Split `command` the way Windows reads a command line.
///
/// Quoted runs -- `'...'` as the shells here accept, `"..."` as Windows does --
/// are one word with the quotes taken off, an unmatched quote is not a command
/// line at all, and a backslash stands for itself except in front of a quote it
/// is escaping.
#[cfg(windows)]
fn split_windows_command_line(command: &str) -> Option<Vec<String>> {
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        match quote {
            Some(open) if c == open => quote = None,
            Some('"') if c == '\\' && chars.peek() == Some(&'"') => {
                word.push('"');
                chars.next();
            }
            Some(_) => word.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                c if c.is_whitespace() => {
                    if started {
                        words.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                c => {
                    word.push(c);
                    started = true;
                }
            },
        }
    }

    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
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

    /// A shell written as a Windows path is the same shell: the tables are
    /// keyed by the name it answers to, not by how it was spelled.
    #[test]
    fn shell_name_is_the_name_a_shell_answers_to() {
        assert_eq!(shell_name(r"C:\Program Files\Git\bin\bash.EXE"), "bash");
        assert_eq!(shell_name("/usr/bin/zsh"), "zsh");
        assert_eq!(shell_name("PowerShell.exe"), "powershell");
        assert_eq!(shell_name("cmd.exe"), "cmd");
        assert_eq!(shell_name("nu"), "nu");
    }

    /// A pane's shell is started the way its own profile expects: a shell that
    /// reads a profile arrives as a login shell, and an interpreter with no such
    /// flag arrives as itself.
    #[cfg(windows)]
    #[test]
    fn interactive_shell_argv_logs_in_only_for_a_profile_reading_shell() {
        assert_eq!(
            interactive_shell_argv(r"C:\Program Files\Git\bin\bash.exe"),
            vec![r"C:\Program Files\Git\bin\bash.exe", "-l"]
        );
        assert_eq!(interactive_shell_argv("/bin/zsh"), vec!["/bin/zsh", "-l"]);
        assert_eq!(interactive_shell_argv("cmd.exe"), vec!["cmd.exe"]);
        assert_eq!(interactive_shell_argv("pwsh.exe"), vec!["pwsh.exe"]);
    }

    /// The null-device suffix is only valid for the interpreter that reads the
    /// deferred script, so keep the two in sync.
    #[test]
    fn silent_output_suffix_matches_the_deferred_interpreter() {
        let program = crate::multiplexer::util::deferred_script_command("echo hi")
            .get_program()
            .to_string_lossy()
            .to_ascii_lowercase();
        if cfg!(windows) {
            assert!(
                program.starts_with("powershell"),
                "unexpected deferred interpreter: {program}"
            );
            assert_eq!(silent_output_suffix(), "> $null 2>&1");
        } else {
            assert_eq!(program, "nohup");
            assert_eq!(silent_output_suffix(), ">/dev/null 2>&1");
        }
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

    /// The shell `snippet_argv` names has to be able to read what
    /// `snippet_quote` writes: cmd rejects the POSIX single-quoted form, and a
    /// path stays bare so that a hand-off which escapes quotes still delivers it.
    #[test]
    fn snippet_quote_matches_the_snippet_shell() {
        if cfg!(windows) {
            assert_eq!(snippet_quote("plain"), "plain");
            assert_eq!(
                snippet_quote(r"C:\workmux\workmux.exe"),
                r"C:\workmux\workmux.exe"
            );
            assert_eq!(
                snippet_quote(r"C:\Program Files\workmux.exe"),
                r#""C:\Program Files\workmux.exe""#
            );
            assert_eq!(snippet_quote("a & b"), "\"a & b\"");
            assert_eq!(snippet_quote(""), "\"\"");
        } else {
            assert_eq!(snippet_quote("plain"), "plain");
            assert_eq!(snippet_quote("a b"), "'a b'");
            assert_eq!(snippet_quote("it's"), "'it'\\''s'");
            assert_eq!(snippet_quote(""), "''");
        }
    }

    /// `cmd.exe` re-parses its own command line, so a snippet containing quotes
    /// has to reach it verbatim rather than through C runtime quoting.
    #[cfg(windows)]
    #[test]
    fn append_snippet_hands_cmd_its_snippet_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out file.txt");
        let script = format!("echo compatible > \"{}\"", output.display());

        let mut command = Command::new("cmd.exe");
        command.args(["/C"]);
        append_snippet(&mut command, "cmd.exe", &script);

        assert!(command.status().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(&output).unwrap().trim(),
            "compatible"
        );
    }

    /// A snippet that opens with a quoted program name is the one cmd.exe
    /// mangles: more than two quotes and a leading quote make it drop the
    /// first and the last, which cuts the program name in half.
    #[cfg(windows)]
    #[test]
    fn append_snippet_keeps_a_quoted_program_name_whole() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("ran.txt");
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        let script = format!("\"{comspec}\" /C echo ran > \"{}\"", output.display());

        let mut command = Command::new("cmd.exe");
        command.args(["/C"]);
        append_snippet(&mut command, "cmd.exe", &script);

        assert!(command.status().unwrap().success());
        assert_eq!(std::fs::read_to_string(&output).unwrap().trim(), "ran");
    }

    #[test]
    fn append_snippet_passes_other_shells_a_single_argument() {
        let script = "echo \"quoted\"";
        let mut command = Command::new("workmux-unused-shell");
        append_snippet(&mut command, "pwsh", script);

        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec![script.to_string()]);
    }

    /// A command line names a program and its arguments, and on Windows the
    /// program is usually a path: splitting it the POSIX way eats the
    /// separators out of that path and leaves a name no shell can find.
    #[test]
    fn a_command_line_splits_the_way_this_platform_reads_it() {
        if cfg!(windows) {
            assert_eq!(
                split_command_line(r"C:\Users\me\fake-bin\claude --verbose --model opus"),
                Some(vec![
                    r"C:\Users\me\fake-bin\claude".to_string(),
                    "--verbose".to_string(),
                    "--model".to_string(),
                    "opus".to_string(),
                ])
            );
        } else {
            assert_eq!(
                split_command_line("/home/me/fake-bin/claude --verbose --model opus"),
                Some(vec![
                    "/home/me/fake-bin/claude".to_string(),
                    "--verbose".to_string(),
                    "--model".to_string(),
                    "opus".to_string(),
                ])
            );
        }

        // Quoted runs are one word, with the quotes taken off.
        assert_eq!(
            split_command_line(r#""C:\Program Files\claude" -p 'a b'"#),
            Some(vec![
                r"C:\Program Files\claude".to_string(),
                "-p".to_string(),
                "a b".to_string(),
            ])
        );
        assert_eq!(split_command_line("   "), Some(vec![]));
        assert_eq!(split_command_line(""), Some(vec![]));
        // A quote that never closes is not a command line.
        assert_eq!(split_command_line("claude 'unclosed"), None);
    }

    /// A word is quoted for the shell that will read it, and left bare when
    /// that shell reads it as itself: a Windows path is one word, not a string.
    #[test]
    fn a_word_is_quoted_for_the_shell_that_reads_it() {
        assert_eq!(
            word_quote(
                r"C:\Users\me\fake-bin\claude",
                ShellDialect::PowerShell,
                true
            ),
            r"C:\Users\me\fake-bin\claude"
        );
        assert_eq!(
            word_quote(r"C:\Users\me\fake-bin\claude", ShellDialect::Posix, true),
            r"C:\Users\me\fake-bin\claude"
        );
        assert_eq!(
            word_quote(r"C:\Users\me\fake-bin\claude", ShellDialect::Cmd, true),
            r"C:\Users\me\fake-bin\claude"
        );

        // A word that has to be quoted: PowerShell names a quoted program with
        // the call operator, and an argument needs no such thing.
        assert_eq!(
            word_quote(r"C:\Program Files\claude", ShellDialect::PowerShell, true),
            r"& 'C:\Program Files\claude'"
        );
        assert_eq!(word_quote("a b", ShellDialect::PowerShell, false), "'a b'");
        assert_eq!(word_quote("a b", ShellDialect::Posix, true), "'a b'");
        assert_eq!(word_quote("a b", ShellDialect::Posix, false), "'a b'");
        assert_eq!(word_quote("it's", ShellDialect::Posix, false), r"'it'\''s'");
        assert_eq!(
            word_quote("it's", ShellDialect::PowerShell, false),
            "'it''s'"
        );
        assert_eq!(word_quote("a & b", ShellDialect::Cmd, false), "\"a & b\"");
    }
}
