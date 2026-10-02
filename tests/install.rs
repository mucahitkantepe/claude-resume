//! Packaging contracts: the installer and the release workflow must agree with each other and
//! with the binary, and the installer must refuse anything it cannot verify.

mod common;

use common::Sandbox;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> String {
    fs::read_to_string(repo().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn install_instructions_point_at_the_real_default_branch() {
    // 0.4 pointed at .../main/install.sh, which is a 404: the default branch is master.
    let url = "https://raw.githubusercontent.com/mucahitkantepe/claude-resume/master/install.sh";
    for file in ["README.md", "install.sh"] {
        let text = read(file);
        assert!(text.contains(url), "{file} should reference {url}");
        assert!(
            !text.contains("/main/install.sh"),
            "{file} references the main branch"
        );
    }
}

#[test]
fn installer_downloads_the_asset_names_the_release_workflow_publishes() {
    // 0.4 downloaded recall-<target>.tar.gz while releases ship claude-resume-<target>.tar.gz.
    let release = read(".github/workflows/release.yml");
    let installer = read("install.sh");
    assert!(release.contains("claude-resume-${{ matrix.target }}.tar.gz"));
    assert!(
        release.contains("claude-resume-${{ matrix.target }}.tar.gz.sha256"),
        "checksums are published"
    );
    assert!(installer.contains("BIN_NAME=\"claude-resume\""));
    assert!(installer.contains("ASSET=\"${BIN_NAME}-${ARCH}-${OS}.tar.gz\""));
    for target in [
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
    ] {
        assert!(
            release.contains(&format!("target: {target}")),
            "release builds {target}"
        );
    }
}

/// A fake GitHub release on disk: a tarball with a stub `claude-resume` that records `init`.
fn fake_release(sb: &Sandbox, corrupt_checksum: bool) -> PathBuf {
    let dir = sb.root.join("release");
    let stage = sb.root.join("stage");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(&stage).unwrap();
    let stub = stage.join("claude-resume");
    fs::write(&stub, format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'claude-resume 9.9.9'; else echo \"$*\" >> '{}'; fi\n", sb.root.join("stub.log").display())).unwrap();
    common::make_executable(&stub);
    let (os, arch) = (
        if cfg!(target_os = "macos") {
            "apple-darwin"
        } else {
            "unknown-linux-gnu"
        },
        std::env::consts::ARCH,
    );
    let asset = format!("claude-resume-{arch}-{os}.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(dir.join(&asset))
        .arg("-C")
        .arg(&stage)
        .arg("claude-resume")
        .status()
        .unwrap();
    assert!(status.success());
    let sum = Command::new("shasum")
        .args(["-a", "256"])
        .arg(dir.join(&asset))
        .output()
        .unwrap();
    let mut line = String::from_utf8(sum.stdout).unwrap();
    if corrupt_checksum {
        line.replace_range(0..1, if line.starts_with('0') { "1" } else { "0" });
    }
    fs::write(dir.join(format!("{asset}.sha256")), line).unwrap();
    dir
}

fn run_installer(sb: &Sandbox, release: &Path, extra: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new("sh");
    cmd.arg(repo().join("install.sh"))
        .env("HOME", &sb.home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env(
            "CLAUDE_RESUME_DOWNLOAD_URL",
            format!("file://{}", release.display()),
        )
        .env("CLAUDE_RESUME_INSTALL_DIR", sb.root.join("installed"));
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

#[cfg(unix)]
#[test]
fn installer_end_to_end_with_checksum_and_init() {
    let sb = Sandbox::new();
    let release = fake_release(&sb, false);
    let out = run_installer(&sb, &release, &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("Checksum verified."));
    assert!(stdout.contains("Installed claude-resume 9.9.9"));
    assert!(stdout.contains("is not on your PATH"));
    assert!(sb.root.join("installed/claude-resume").exists());
    assert_eq!(
        fs::read_to_string(sb.root.join("stub.log")).unwrap(),
        "init\n",
        "init configures Claude Code"
    );
}

#[cfg(unix)]
#[test]
fn installer_can_skip_init() {
    let sb = Sandbox::new();
    let release = fake_release(&sb, false);
    let out = run_installer(&sb, &release, &[("CLAUDE_RESUME_NO_INIT", "1")]);
    assert!(out.status.success());
    assert!(!sb.root.join("stub.log").exists());
}

#[cfg(unix)]
#[test]
fn installer_refuses_a_tampered_download() {
    let sb = Sandbox::new();
    let release = fake_release(&sb, true);
    let out = run_installer(&sb, &release, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Checksum mismatch"));
    assert!(!sb.root.join("installed/claude-resume").exists());
}

#[cfg(unix)]
#[test]
fn installer_refuses_a_download_it_cannot_verify() {
    let sb = Sandbox::new();
    let release = fake_release(&sb, false);
    for entry in fs::read_dir(&release).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "sha256") {
            fs::remove_file(path).unwrap();
        }
    }
    let out = run_installer(&sb, &release, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing was installed"));
    assert!(!sb.root.join("installed/claude-resume").exists());
}
