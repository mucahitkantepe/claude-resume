//! Resuming a session: go to the directory it was started in, then `claude --resume <id>`.

use anyhow::{Result, ensure};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

/// Session ids are file names under `projects/`. They are interpolated into a shell command, so
/// anything beyond a conservative character set is refused rather than quoted.
pub fn valid_sid(sid: &str) -> bool {
    !sid.is_empty()
        && sid.len() <= 128
        && !sid.starts_with(['-', '.'])
        && sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub sid: String,
    /// Where to run `claude`: the session's original directory, if it still exists.
    pub dir: Option<PathBuf>,
    /// The original directory, when it no longer exists (Claude Code ≥ 2.1.223 can still resume
    /// the session from anywhere).
    pub missing_dir: Option<String>,
}

pub fn plan(sid: &str, cwd: &str) -> Result<Plan> {
    ensure!(
        valid_sid(sid),
        "refusing to resume {sid:?}: not a valid session id"
    );
    let (dir, missing_dir) = match cwd {
        "" => (None, None),
        d if std::path::Path::new(d).is_dir() => (Some(PathBuf::from(d)), None),
        d => (None, Some(d.to_string())),
    };
    Ok(Plan {
        sid: sid.to_string(),
        dir,
        missing_dir,
    })
}

impl Plan {
    /// The command to paste into a terminal.
    pub fn command_line(&self) -> String {
        match &self.dir {
            Some(dir) => format!(
                "cd {} && claude --resume {}",
                shell_quote(&dir.to_string_lossy()),
                self.sid
            ),
            None => format!("claude --resume {}", self.sid),
        }
    }

    /// Run `claude --resume` through the user's interactive shell, so a `claude` alias or
    /// function (e.g. one adding flags) is honoured. The `cd` happens inside the shell, after its
    /// startup files ran, so an rc file that changes directory can't send us elsewhere.
    pub fn run(&self) -> Result<ExitStatus> {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut cmd = Command::new(&shell);
        cmd.arg("-ic").arg(self.command_line());
        if let Some(dir) = &self.dir {
            cmd.current_dir(dir);
        }
        Ok(cmd.status()?)
    }
}

/// Quote for POSIX shells (and fish): bare when safe, single-quoted otherwise.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./+:@%,=".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_validation() {
        assert!(valid_sid("1b2c3d4e-5f60-7182-93a4-b5c6d7e8f901"));
        assert!(valid_sid("agent-a1b2c3"));
        for bad in [
            "",
            "-rf",
            ".hidden",
            "a b",
            "x;rm -rf ~",
            "$(id)",
            "a/b",
            "é",
            &"a".repeat(129),
        ] {
            assert!(!valid_sid(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn plan_uses_existing_dir_and_reports_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan("abc", dir.path().to_str().unwrap()).unwrap();
        assert_eq!(p.dir.as_deref(), Some(dir.path()));
        let gone = plan("abc", "/no/such/dir/anymore").unwrap();
        assert_eq!(
            (gone.dir, gone.missing_dir.as_deref()),
            (None, Some("/no/such/dir/anymore"))
        );
        assert!(plan("bad id", "/").is_err());
    }

    #[test]
    fn command_line_quotes_paths() {
        let p = Plan {
            sid: "abc".into(),
            dir: Some("/home/me/my project's".into()),
            missing_dir: None,
        };
        assert_eq!(
            p.command_line(),
            r"cd '/home/me/my project'\''s' && claude --resume abc"
        );
        let p = Plan {
            sid: "abc".into(),
            dir: Some("/srv/app".into()),
            missing_dir: None,
        };
        assert_eq!(p.command_line(), "cd /srv/app && claude --resume abc");
        let p = Plan {
            sid: "abc".into(),
            dir: None,
            missing_dir: None,
        };
        assert_eq!(p.command_line(), "claude --resume abc");
    }
}
