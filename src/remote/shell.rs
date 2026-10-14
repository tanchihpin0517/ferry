//! Building commands for a host's shell.
//!
//! `ssh` joins its arguments into one string for the user's login shell,
//! whatever that shell is. So every command Ferry runs on a host is
//! `sh -c '<script>'`, and the script is POSIX shell.

/// Quotes a string for a POSIX shell, so that it is one word, taken literally.
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The command line that runs `script` with `sh` on a host.
pub fn sh_command(script: &str) -> String {
    format!("sh -c {}", quote(script))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn quoting() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote(""), "''");
    }

    /// Runs a command line the way the login shell on a host would.
    fn run(command_line: &str) -> String {
        let output = Command::new("sh")
            .arg("-c")
            .arg(command_line)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn a_script_survives_the_login_shell() {
        let script = r#"printf '%s|' "it's" "$HOME" '$x' "a  b""#;
        let home = std::env::var("HOME").unwrap();
        assert_eq!(run(&sh_command(script)), format!("it's|{home}|$x|a  b|"));
    }
}
