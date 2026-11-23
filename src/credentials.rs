use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use anyhow::{bail, Context, Result};

pub fn terminal_disabled(value: Option<&OsStr>) -> bool {
    let Some(value) = value else { return false };
    match value.to_str().map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("true" | "1" | "yes" | "on") => true,
        Some("false" | "0" | "no" | "off") => false,
        _ => {
            eprintln!("invalid MONITOR_DISABLE_TERMINAL; Web Terminal disabled");
            true
        }
    }
}

pub fn read_token(path: &Path) -> Result<String> {
    // Inspect the opened descriptor, never a pathname that can be switched after validation.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("open node token file")?;
    let info = file.metadata()?;
    if !info.is_file()
        || info.mode() & 0o077 != 0
        || (info.uid() != 0 && info.uid() != unsafe { libc::geteuid() })
    {
        bail!("token file must be a private regular file owned by root or the agent user");
    }
    const LIMIT: u64 = 4096;
    let mut data = String::new();
    file.take(LIMIT + 1).read_to_string(&mut data).context("read node token file")?;
    let token = data.trim_end_matches(['\r', '\n']);
    if data.len() as u64 > LIMIT || token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        bail!("token file must contain a single nonempty token of at most 4096 bytes");
    }
    Ok(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, Permissions};
    use std::io::Write;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn explicit_invalid_terminal_policy_fails_closed() {
        assert!(!terminal_disabled(None));
        assert!(!terminal_disabled(Some(OsStr::new("  OFF "))));
        for value in [" Yes ", "1", "", "typo"] {
            assert!(terminal_disabled(Some(OsStr::new(value))));
        }
    }

    #[test]
    fn credentials_require_a_private_regular_file() {
        let folder = std::env::temp_dir().join(format!("cagent-credential-test-{}", std::process::id()));
        fs::create_dir(&folder).unwrap();
        let path = folder.join("token");
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).unwrap();
        file.write_all(b"fixture-only\n").unwrap();
        assert_eq!(read_token(&path).unwrap(), "fixture-only");
        symlink(&path, folder.join("link")).unwrap();
        assert!(read_token(&folder.join("link")).is_err());
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        assert!(read_token(&path).is_err());
        fs::remove_dir_all(folder).unwrap();
    }
}
