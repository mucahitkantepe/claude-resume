use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn settings_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".claude")
        .join("settings.json")
}

/// Patch ~/.claude/settings.json to set cleanupPeriodDays and add the SessionStart hook.
pub fn patch_settings() -> Result<(), String> {
    let path = settings_path();

    let mut settings: Value = if path.exists() {
        let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        serde_json::from_str(&content).map_err(|e| e.to_string())?
    } else {
        // Create ~/.claude/ if needed
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        serde_json::json!({})
    };

    let obj = settings.as_object_mut().ok_or("settings is not an object")?;

    // Set cleanupPeriodDays
    let current = obj.get("cleanupPeriodDays").and_then(|v| v.as_u64());
    if current.unwrap_or(0) < 99999 {
        obj.insert(
            "cleanupPeriodDays".to_string(),
            serde_json::json!(99999),
        );
        eprintln!("  ✓ Set cleanupPeriodDays: 99999 (sessions preserved indefinitely)");
    } else {
        eprintln!("  ✓ cleanupPeriodDays already set");
    }

    // Add SessionStart hook for auto-sync
    let recall_bin = which_recall();
    let hook_command = format!("{} sync &", recall_bin);

    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks.as_object_mut().ok_or("hooks is not an object")?;

    let session_start = hooks_obj
        .entry("SessionStart")
        .or_insert_with(|| serde_json::json!([]));
    let session_arr = session_start
        .as_array_mut()
        .ok_or("SessionStart is not an array")?;

    // Check if our hook already exists
    let already_has_hook = session_arr.iter().any(|h| {
        h.get("hooks")
            .and_then(|hks| hks.as_array())
            .map(|arr| {
                arr.iter().any(|hk| {
                    hk.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("claude-resume"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !already_has_hook {
        session_arr.push(serde_json::json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": hook_command,
                "timeout": 10
            }]
        }));
        eprintln!("  ✓ Added SessionStart hook for auto-sync");
    } else {
        eprintln!("  ✓ SessionStart hook already configured");
    }

    // Register plugin
    let marketplaces = obj
        .entry("extraKnownMarketplaces")
        .or_insert_with(|| serde_json::json!({}));
    let mp_obj = marketplaces.as_object_mut().ok_or("extraKnownMarketplaces is not an object")?;

    if !mp_obj.contains_key("claude-resume") {
        mp_obj.insert(
            "claude-resume".to_string(),
            serde_json::json!({
                "source": {
                    "source": "github",
                    "repo": "mucahitkantepe/claude-resume"
                }
            }),
        );
        eprintln!("  ✓ Registered claude-resume plugin (enables /claude-resume:search in Claude Code)");
    } else {
        eprintln!("  ✓ Plugin already registered");
    }

    // Write back
    let output = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    fs::write(&path, output).map_err(|e| e.to_string())?;

    Ok(())
}

/// Find the recall binary path.
fn which_recall() -> String {
    // Check if we're in ~/.local/bin
    let local_bin = dirs::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("bin")
        .join("claude-resume");
    if local_bin.exists() {
        return local_bin.to_string_lossy().to_string();
    }
    // Fallback: assume it's in PATH
    "claude-resume".to_string()
}

/// Uninstall: remove hook and reset cleanupPeriodDays.
pub fn unpatch_settings() -> Result<(), String> {
    let path = settings_path();
    if !path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut settings: Value = serde_json::from_str(&content).map_err(|e| e.to_string())?;

    if let Some(obj) = settings.as_object_mut() {
        // Remove our hook
        if let Some(hooks) = obj.get_mut("hooks") {
            if let Some(hooks_obj) = hooks.as_object_mut() {
                if let Some(session_start) = hooks_obj.get_mut("SessionStart") {
                    if let Some(arr) = session_start.as_array_mut() {
                        arr.retain(|h| {
                            !h.get("hooks")
                                .and_then(|hks| hks.as_array())
                                .map(|a| {
                                    a.iter().any(|hk| {
                                        hk.get("command")
                                            .and_then(|c| c.as_str())
                                            .map(|c| c.contains("claude-resume"))
                                            .unwrap_or(false)
                                    })
                                })
                                .unwrap_or(false)
                        });
                    }
                }
            }
        }

        // Remove plugin registration
        if let Some(mp) = obj.get_mut("extraKnownMarketplaces") {
            if let Some(mp_obj) = mp.as_object_mut() {
                mp_obj.remove("claude-resume");
            }
        }

        eprintln!("  ✓ Removed SessionStart hook");
        eprintln!("  ✓ Removed plugin registration");
        eprintln!("  ℹ cleanupPeriodDays left as-is (your sessions are safe)");
    }

    let output = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    fs::write(&path, output).map_err(|e| e.to_string())?;

    Ok(())
}
