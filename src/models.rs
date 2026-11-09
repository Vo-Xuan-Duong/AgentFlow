//! Per-repository model overrides for the *CLI invocations AgentFlow owns*.
//! These settings do not control the external Codex Desktop app session.
use crate::state_root;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::{Path, PathBuf}};

const MAX_CONFIG_BYTES: u64 = 16 * 1024;
const MAX_SLUG_BYTES: usize = 128;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelSettings {
    /// Optional --model argument for Codex CLI planning/reviewing only.
    /// Codex Desktop selects its own model inside its app.
    #[serde(default)]
    pub codex_cli: Option<String>,
    /// Optional --model argument for Antigravity CLI headless execution.
    #[serde(default)]
    pub antigravity: Option<String>,
}

pub fn path(root: &Path) -> Result<PathBuf> {
    Ok(state_root(root)?.join("models.json"))
}

pub fn is_valid_slug(value: &str) -> bool {
    value.len() <= MAX_SLUG_BYTES
        && value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':' | b'+')
        })
}

pub fn validate(settings: &ModelSettings) -> Result<()> {
    for (name, value) in [
        ("codex_cli", &settings.codex_cli),
        ("antigravity", &settings.antigravity),
    ] {
        if let Some(value) = value {
            if !is_valid_slug(value) {
                bail!(
                    "invalid {name} model slug; use a model ID of at most {MAX_SLUG_BYTES} ASCII characters (letters, digits, . _ - / : +)"
                );
            }
        }
    }
    Ok(())
}

pub fn load(root: &Path) -> Result<ModelSettings> {
    let file = path(root)?;
    let bytes = match fs::metadata(&file) {
        Ok(meta) if meta.len() <= MAX_CONFIG_BYTES && meta.is_file() => {
            fs::read(&file).with_context(|| format!("cannot read {}", file.display()))?
        }
        Ok(_) => bail!("model config is not a regular JSON file under 16 KiB: {}", file.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ModelSettings::default());
        }
        Err(error) => return Err(error).with_context(|| format!("cannot inspect {}", file.display())),
    };
    let settings: ModelSettings = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid JSON model config at {}", file.display()))?;
    validate(&settings)?;
    Ok(settings)
}

pub fn save(root: &Path, settings: &ModelSettings) -> Result<()> {
    validate(settings)?;
    let dest = path(root)?;
    let folder = dest.parent().context("model config has no parent")?;
    fs::create_dir_all(folder)?;
    // Keep configuration outside the Git checkout and swap using files in
    // the same folder. On Windows an existing target must be moved first.
    let temp = folder.join("models.json.tmp");
    let backup = folder.join("models.json.bak");
    let mut output = fs::File::create(&temp)?;
    output.write_all(&serde_json::to_vec_pretty(settings)?)?;
    output.sync_all()?;
    drop(output);
    if dest.exists() {
        if backup.exists() { fs::remove_file(&backup)?; }
        fs::rename(&dest, &backup)?;
        if let Err(error) = fs::rename(&temp, &dest) {
            let _ = fs::rename(&backup, &dest);
            return Err(error).context("could not replace model config");
        }
        let _ = fs::remove_file(backup);
    } else {
        fs::rename(&temp, &dest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("agentflow-models-{}-{stamp}", std::process::id()))
    }

    #[test]
    fn model_slug_validation_is_strict() {
        assert!(is_valid_slug("gemini-3.8-flash-medium"));
        assert!(is_valid_slug("gpt-6.1-sol"));
        assert!(is_valid_slug("provider/model:v1+high"));
        for invalid in ["", "-p", "--dangerously-skip-permissions", "gemini model",
                        "model\nanother", "🐱"] {
            assert!(!is_valid_slug(invalid), "accepted invalid slug: {invalid:?}");
        }
    }

    #[test]
    fn settings_roundtrip_and_defaults() {
        let folder = sandbox();
        let root = folder.join("repo");
        fs::create_dir_all(&root).unwrap();
        assert_eq!(load(&root).unwrap(), ModelSettings::default());
        let selected = ModelSettings {
            codex_cli: Some("gpt-6.1-sol".into()),
            antigravity: Some("gemini-3.8-flash-medium".into()),
        };
        save(&root, &selected).unwrap();
        assert_eq!(load(&root).unwrap(), selected);
        let updated = ModelSettings {
            codex_cli: None,
            antigravity: Some("gemini-3.1-pro-high".into()),
        };
        save(&root, &updated).unwrap();
        assert_eq!(load(&root).unwrap(), updated);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn rejects_malformed_or_unknown_config() {
        let folder = sandbox();
        let root = folder.join("repo");
        fs::create_dir_all(&root).unwrap();
        let dest = path(&root).unwrap();
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, r#"{"unexpected":"value"}"#).unwrap();
        assert!(load(&root).is_err());
        fs::write(&dest, r#"{"antigravity":"--model"}"#).unwrap();
        assert!(load(&root).is_err());
        fs::remove_dir_all(folder).unwrap();
    }
}
