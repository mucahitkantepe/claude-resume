//! `init` and `uninstall`: the changes claude-resume makes to Claude Code's configuration.
//!
//! `settings.json` is edited in place with its key order, indentation and unrelated content
//! preserved, written atomically (through a symlink, if it is one), and backed up once before the
//! first change.

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Effectively "never": Claude Code deletes transcripts older than this many days (default 30).
pub const RETENTION_DAYS: u64 = 99_999;
/// The plugin marketplace claude-resume ≤ 0.4 registered in settings.json.
const LEGACY_MARKETPLACE: &str = "claude-resume";

pub struct Settings {
    /// The file itself, with symlinks followed.
    path: PathBuf,
    value: Value,
    original: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Retention {
    AlreadySet(u64),
    Set { previous: Option<u64> },
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        let path = &resolve(path);
        let original = match fs::read_to_string(path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let value = match original.as_deref().map(str::trim) {
            None | Some("") => json!({}),
            Some(s) => serde_json::from_str(s).with_context(|| {
                format!(
                    "{} is not valid JSON; fix it (or move it aside) and run this again",
                    path.display()
                )
            })?,
        };
        ensure!(
            value.is_object(),
            "{} does not contain a JSON object",
            path.display()
        );
        Ok(Self {
            path: path.to_path_buf(),
            value,
            original,
        })
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    fn obj(&mut self) -> &mut Map<String, Value> {
        self.value.as_object_mut().expect("checked in load")
    }

    /// Keep transcripts (practically) forever: Claude Code deletes them after 30 days by default.
    pub fn ensure_retention(&mut self) -> Retention {
        let current = self.value.get("cleanupPeriodDays").and_then(Value::as_u64);
        match current {
            Some(days) if days >= RETENTION_DAYS => Retention::AlreadySet(days),
            previous => {
                self.obj()
                    .insert("cleanupPeriodDays".into(), json!(RETENTION_DAYS));
                Retention::Set { previous }
            }
        }
    }

    /// Remove the `SessionStart` hook claude-resume ≤ 0.4 added (`…/claude-resume sync &`); every
    /// command now brings the index up to date itself. Returns how many were removed.
    pub fn remove_legacy_hooks(&mut self) -> usize {
        let is_ours = |hook: &Value| {
            hook.get("command")
                .and_then(Value::as_str)
                .is_some_and(|c| c.contains("claude-resume") && c.contains(" sync"))
        };
        let Some(hooks) = self.obj().get_mut("hooks").and_then(Value::as_object_mut) else {
            return 0;
        };
        let Some(groups) = hooks.get_mut("SessionStart").and_then(Value::as_array_mut) else {
            return 0;
        };
        let mut removed = 0;
        for group in groups.iter_mut() {
            if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = list.len();
                list.retain(|h| !is_ours(h));
                removed += before - list.len();
            }
        }
        groups.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|l| !l.is_empty())
        });
        if groups.is_empty() {
            hooks.shift_remove("SessionStart");
        }
        if hooks.is_empty() {
            self.obj().shift_remove("hooks");
        }
        removed
    }

    /// Remove the `extraKnownMarketplaces.claude-resume` entry claude-resume ≤ 0.4 added.
    pub fn remove_marketplace_entry(&mut self) -> bool {
        let Some(m) = self
            .obj()
            .get_mut("extraKnownMarketplaces")
            .and_then(Value::as_object_mut)
        else {
            return false;
        };
        let removed = m.shift_remove(LEGACY_MARKETPLACE).is_some();
        if m.is_empty() {
            self.obj().shift_remove("extraKnownMarketplaces");
        }
        removed
    }

    pub fn changed(&self) -> bool {
        let before = match self.original.as_deref().map(str::trim) {
            None | Some("") => json!({}),
            Some(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        };
        before != self.value
    }

    /// Write the settings back if anything changed. The first time, the original file is copied
    /// to `settings.json.claude-resume.bak`. Returns the backup path when one was made.
    pub fn save(&self) -> Result<Option<PathBuf>> {
        if !self.changed() {
            return Ok(None);
        }
        if fs::metadata(&self.path).is_ok_and(|m| m.permissions().readonly()) {
            bail!(
                "{} is read-only (managed by another tool?); make the change there yourself",
                self.path.display()
            );
        }
        let mut backup = None;
        if let Some(original) = &self.original {
            let path = backup_path(&self.path);
            if !path.exists() {
                fs::write(&path, original)
                    .with_context(|| format!("writing {}", path.display()))?;
                backup = Some(path);
            }
        }
        let indent = self
            .original
            .as_deref()
            .and_then(indentation)
            .unwrap_or("  ");
        let mut body = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
        self.value
            .serialize(&mut serde_json::Serializer::with_formatter(
                &mut body, formatter,
            ))?;
        body.push(b'\n');
        write_atomic(&self.path, &body)?;
        Ok(backup)
    }
}

/// Follow `path` if it is a symlink (dotfiles managers link settings.json into a repository).
/// Replacing the link with a file would detach it, and the next run of the manager would put
/// the old settings back. A dangling link is followed too: the file belongs at its target.
fn resolve(path: &Path) -> PathBuf {
    let mut path = path.to_path_buf();
    for _ in 0..40 {
        match fs::read_link(&path) {
            Ok(target) => path = path.parent().unwrap_or(Path::new("")).join(target),
            Err(_) => break,
        }
    }
    path
}

/// The indentation of the first indented line of a JSON document.
fn indentation(json: &str) -> Option<&str> {
    json.lines().find_map(|line| {
        let rest = line.trim_start_matches([' ', '\t']);
        let indent = &line[..line.len() - rest.len()];
        (!indent.is_empty() && !rest.is_empty()).then_some(indent)
    })
}

pub fn backup_path(settings: &Path) -> PathBuf {
    let mut name = settings.file_name().unwrap_or_default().to_os_string();
    name.push(".claude-resume.bak");
    settings.with_file_name(name)
}

/// Replace `path` with `data` without ever leaving a half-written file behind, keeping the
/// original file's permissions.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        if let Ok(meta) = fs::metadata(path) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(json: &str) -> (tempfile::TempDir, Settings) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, json).unwrap();
        let s = Settings::load(&path).unwrap();
        (dir, s)
    }

    #[test]
    fn retention_is_raised_but_never_lowered() {
        let (_d, mut s) = load(r#"{"cleanupPeriodDays": 30}"#);
        assert_eq!(s.ensure_retention(), Retention::Set { previous: Some(30) });
        assert_eq!(s.value()["cleanupPeriodDays"], RETENTION_DAYS);
        let (_d, mut s) = load(r#"{"cleanupPeriodDays": 100000}"#);
        assert_eq!(s.ensure_retention(), Retention::AlreadySet(100_000));
        let (_d, mut s) = load("{}");
        assert_eq!(s.ensure_retention(), Retention::Set { previous: None });
    }

    #[test]
    fn removes_only_the_legacy_hook() {
        let (_d, mut s) = load(
            r#"{"hooks": {"SessionStart": [
                {"matcher": "", "hooks": [{"type": "command", "command": "/home/me/.local/bin/claude-resume sync &", "timeout": 10}]},
                {"hooks": [{"type": "command", "command": "bash other.sh"}, {"type": "command", "command": "claude-resume sync &"}]}
            ], "Stop": [{"hooks": [{"type": "command", "command": "notify"}]}]}}"#,
        );
        assert_eq!(s.remove_legacy_hooks(), 2);
        let start = &s.value()["hooks"]["SessionStart"];
        assert_eq!(
            start.as_array().unwrap().len(),
            1,
            "emptied group removed, other group kept"
        );
        assert_eq!(start[0]["hooks"][0]["command"], "bash other.sh");
        assert!(s.value()["hooks"]["Stop"].is_array());
        assert_eq!(s.remove_legacy_hooks(), 0, "idempotent");
    }

    #[test]
    fn removing_last_hook_drops_empty_containers() {
        let (_d, mut s) = load(
            r#"{"a": 1, "hooks": {"SessionStart": [{"hooks": [{"command": "x/claude-resume sync &"}]}]}}"#,
        );
        assert_eq!(s.remove_legacy_hooks(), 1);
        assert_eq!(s.value(), &json!({"a": 1}));
    }

    #[test]
    fn save_preserves_key_order_backs_up_once_and_is_noop_when_unchanged() {
        let original =
            "{\n  \"zeta\": 1,\n  \"alpha\": {\"y\": 2, \"x\": 3},\n  \"cleanupPeriodDays\": 30\n}";
        let (dir, mut s) = load(original);
        assert_eq!(s.save().unwrap(), None, "nothing changed yet");
        s.ensure_retention();
        let backup = s.save().unwrap().expect("backup made");
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);
        let written = fs::read_to_string(dir.path().join("settings.json")).unwrap();
        let keys: Vec<&str> = [
            "\"zeta\"",
            "\"alpha\"",
            "\"y\"",
            "\"x\"",
            "\"cleanupPeriodDays\"",
        ]
        .into_iter()
        .filter(|k| written.contains(k))
        .collect();
        assert_eq!(keys.len(), 5);
        let pos = |k: &str| written.find(k).unwrap();
        assert!(
            pos("\"zeta\"") < pos("\"alpha\"") && pos("\"y\"") < pos("\"x\""),
            "order kept:\n{written}"
        );
        // A second change must not overwrite the first backup.
        let mut s = Settings::load(&dir.path().join("settings.json")).unwrap();
        s.obj().insert("new".into(), json!(true));
        assert_eq!(s.save().unwrap(), None);
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);
    }

    #[test]
    fn missing_file_is_created_without_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        let mut s = Settings::load(&path).unwrap();
        s.ensure_retention();
        assert_eq!(s.save().unwrap(), None);
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v, json!({"cleanupPeriodDays": RETENTION_DAYS}));
    }

    #[test]
    fn invalid_json_is_an_error_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "{ not json").unwrap();
        let err = Settings::load(&path).err().unwrap();
        assert!(format!("{err:#}").contains("not valid JSON"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
        fs::write(&path, "[1, 2]").unwrap();
        assert!(Settings::load(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, mut s) = load("{}");
        let path = dir.path().join("settings.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        s.ensure_retention();
        s.save().unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn removals_keep_the_order_and_indentation_of_everything_else() {
        let original = "{\n    \"alpha\": 1,\n    \"hooks\": {\"SessionStart\": [{\"hooks\": [{\"command\": \"claude-resume sync &\"}]}]},\n    \"bravo\": 2,\n    \"extraKnownMarketplaces\": {\"claude-resume\": {}},\n    \"charlie\": 3,\n    \"delta\": 4\n}\n";
        let (dir, mut s) = load(original);
        assert_eq!(s.remove_legacy_hooks(), 1);
        assert!(s.remove_marketplace_entry());
        s.ensure_retention();
        s.save().unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("settings.json")).unwrap(),
            "{\n    \"alpha\": 1,\n    \"bravo\": 2,\n    \"charlie\": 3,\n    \"delta\": 4,\n    \"cleanupPeriodDays\": 99999\n}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_is_written_through_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        fs::create_dir_all(&dotfiles).unwrap();
        fs::write(dotfiles.join("settings.json"), r#"{"model": "opus"}"#).unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink("dotfiles/settings.json", &link).unwrap();

        let mut s = Settings::load(&link).unwrap();
        s.ensure_retention();
        let backup = s.save().unwrap().expect("backup made");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let v: Value =
            serde_json::from_str(&fs::read_to_string(dotfiles.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            v,
            json!({"model": "opus", "cleanupPeriodDays": RETENTION_DAYS})
        );
        assert_eq!(backup, dotfiles.join("settings.json.claude-resume.bak"));
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_settings_file_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, mut s) = load(r#"{"cleanupPeriodDays": 30}"#);
        let path = dir.path().join("settings.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        s.ensure_retention();
        let err = s.save().expect_err("refused");
        assert!(format!("{err:#}").contains("read-only"), "{err:#}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            r#"{"cleanupPeriodDays": 30}"#
        );
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn marketplace_entry_removal() {
        let (_d, mut s) = load(
            r#"{"extraKnownMarketplaces": {"claude-resume": {"source": {"source": "github", "repo": "x/y"}}}}"#,
        );
        assert!(s.remove_marketplace_entry());
        assert_eq!(s.value(), &json!({}));
        assert!(!s.remove_marketplace_entry());
    }
}
