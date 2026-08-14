//! 本地 live 配置：从 `eval.env` 读 `OPENAI_*`，不把 key 写进证据包。
//! 进程环境里已有的变量优先，方便 CI / 临时覆盖。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const ALLOWED: &[&str] = &["OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL"];

struct Loaded {
    path: PathBuf,
    values: BTreeMap<String, String>,
}

static LOADED: OnceLock<Option<Loaded>> = OnceLock::new();

/// 查找并解析本地文件。可重复调用；第一次之后复用。
pub fn load() -> anyhow::Result<Option<PathBuf>> {
    if let Some(loaded) = LOADED.get() {
        return Ok(loaded.as_ref().map(|loaded| loaded.path.clone()));
    }
    let loaded = load_inner()?;
    let path = loaded.as_ref().map(|loaded| loaded.path.clone());
    let _ = LOADED.set(loaded);
    Ok(path)
}

/// 进程环境优先，其次 `eval.env`。空字符串视为未设置。
pub fn get(key: &str) -> Option<String> {
    if let Ok(value) = std::env::var(key) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(value);
        }
    }
    LOADED
        .get()
        .and_then(|loaded| loaded.as_ref())
        .and_then(|loaded| loaded.values.get(key).cloned())
        .filter(|value| !value.trim().is_empty())
}

pub fn status_line(path: &Path) -> String {
    let model = get("OPENAI_MODEL").unwrap_or_else(|| "(unset)".into());
    let base = get("OPENAI_BASE_URL").unwrap_or_else(|| "(unset)".into());
    let key = if get("OPENAI_API_KEY").is_some() {
        "present"
    } else {
        "missing"
    };
    format!(
        "eval env: {} model={model} base={base} key={key}",
        path.display()
    )
}

pub fn parse_eval_env(text: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (index, raw) in text.lines().enumerate() {
        let mut line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim();
        }
        let Some((key, value)) = line.split_once('=') else {
            anyhow::bail!("line {}: expected KEY=VALUE", index + 1);
        };
        let key = key.trim();
        if !ALLOWED.contains(&key) {
            anyhow::bail!(
                "line {}: refusing to load {key} (only OPENAI_API_KEY / OPENAI_BASE_URL / OPENAI_MODEL)",
                index + 1
            );
        }
        out.insert(key.to_string(), unquote(value.trim()));
    }
    Ok(out)
}

fn load_inner() -> anyhow::Result<Option<Loaded>> {
    let path = candidate_path();
    let Some(path) = path else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(Loaded {
        values: parse_eval_env(&text)?,
        path,
    }))
}

fn candidate_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("EVAL_ENV_FILE") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
        return None;
    }
    ["eval.env", ".env"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_keys_and_strips_quotes() {
        let map = parse_eval_env(
            "# comment\n\
             export OPENAI_MODEL=\"gpt-5.6-luna\"\n\
             OPENAI_BASE_URL=https://api.pinaic.com/v1\n\
             OPENAI_API_KEY='sk-test'\n",
        )
        .unwrap();
        assert_eq!(
            map.get("OPENAI_MODEL").map(String::as_str),
            Some("gpt-5.6-luna")
        );
        assert_eq!(
            map.get("OPENAI_BASE_URL").map(String::as_str),
            Some("https://api.pinaic.com/v1")
        );
        assert_eq!(
            map.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-test")
        );
    }

    #[test]
    fn rejects_unrelated_keys() {
        let error = parse_eval_env("HOME=/tmp\n").unwrap_err().to_string();
        assert!(error.contains("HOME"), "{error}");
    }

    #[test]
    fn empty_key_is_unset() {
        let map = parse_eval_env("OPENAI_API_KEY=\nOPENAI_MODEL=gpt-4o-mini\n").unwrap();
        assert_eq!(map.get("OPENAI_API_KEY").map(String::as_str), Some(""));
        assert_eq!(
            map.get("OPENAI_MODEL").map(String::as_str),
            Some("gpt-4o-mini")
        );
    }
}
