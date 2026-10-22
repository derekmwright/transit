//! The per-run token that guards the app's loopback control channel.
//!
//! The app (`transit --control PORT`) generates a token at start and writes it to
//! [`token_file_path`]; every connection must send `auth <token>` as its first line. The client
//! ([`crate::Remote`]) reads the same file, so a process running as the same user can connect and
//! a web page or another account cannot.

use std::ffi::OsString;
use std::path::PathBuf;

/// Overrides [`token_file_path`].
pub const TOKEN_FILE_ENV: &str = "TRANSIT_CONTROL_TOKEN_FILE";
/// The file's name inside the app's settings folder.
pub const TOKEN_FILE_NAME: &str = "control-token";
/// The first line of a connection: this word, a space, the token.
pub const AUTH_PREFIX: &str = "auth ";

/// Where the token lives: `$TRANSIT_CONTROL_TOKEN_FILE`, else `control-token` in transit's settings
/// folder (`$TRANSIT_CONFIG_DIR`; else `%APPDATA%\transit`, `~/Library/Application Support/transit`
/// or `$XDG_CONFIG_HOME/transit` / `~/.config/transit`). `None` when neither can be resolved.
pub fn token_file_path() -> Option<PathBuf> {
    path_with(|n| std::env::var_os(n))
}

fn path_with(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(file) = env(TOKEN_FILE_ENV) {
        return (!file.is_empty()).then(|| PathBuf::from(file));
    }
    let dir = if let Some(dir) = env("TRANSIT_CONFIG_DIR") {
        (!dir.is_empty()).then(|| PathBuf::from(dir))?
    } else if cfg!(target_os = "macos") {
        PathBuf::from(env("HOME")?).join("Library/Application Support/transit")
    } else if cfg!(windows) {
        PathBuf::from(env("APPDATA")?).join("transit")
    } else {
        env("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")))?.join("transit")
    };
    Some(dir.join(TOKEN_FILE_NAME))
}

/// A fresh token: 32 hex characters (128 bits) from the operating system's random generator.
pub fn generate() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("no random bytes for the control token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Write `token` to `path`, replacing any earlier one; readable by the current user only where the
/// platform allows (mode 0600 on Unix; on Windows the file inherits its folder's per-user ACL).
pub fn write_token_file(path: &std::path::Path, token: &str) -> Result<(), String> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    // Remove first so an older file with looser permissions does not keep them.
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(token.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// The token in [`token_file_path`], if the file exists and holds one.
pub fn read_token() -> Option<String> {
    let text = std::fs::read_to_string(token_file_path()?).ok()?;
    let token = text.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Compare without stopping at the first differing byte.
pub fn matches(expected: &str, given: &str) -> bool {
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    let mut diff = a.len() ^ b.len();
    for (i, x) in a.iter().enumerate() {
        diff |= usize::from(x ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_32_hex_characters_and_differ() {
        let (a, b) = (generate().unwrap(), generate().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn the_path_follows_the_override_then_the_config_folder() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |n: &str| pairs.iter().find(|(k, _)| *k == n).map(|(_, v)| OsString::from(v));
        assert_eq!(path_with(env(&[(TOKEN_FILE_ENV, "/x/t"), ("TRANSIT_CONFIG_DIR", "/c")])), Some(PathBuf::from("/x/t")));
        assert_eq!(path_with(env(&[("TRANSIT_CONFIG_DIR", "/c")])), Some(PathBuf::from("/c").join(TOKEN_FILE_NAME)));
        assert_eq!(path_with(env(&[(TOKEN_FILE_ENV, "")])), None, "an empty override disables the file");
    }

    #[test]
    fn the_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("transit-token-test-{}", std::process::id()));
        let path = dir.join("nested").join(TOKEN_FILE_NAME);
        write_token_file(&path, "abc123").unwrap();
        write_token_file(&path, "def456").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "def456");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn comparison_needs_equal_tokens() {
        assert!(matches("abc", "abc"));
        assert!(!matches("abc", "abd"));
        assert!(!matches("abc", "abcd"));
        assert!(!matches("abc", ""));
    }
}
