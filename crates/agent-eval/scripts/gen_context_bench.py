"""One-shot generator for context-bench seeds, tasks, goldens, checks."""
from __future__ import annotations

import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1] / "context-bench"


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text.replace("\r\n", "\n"), encoding="utf-8")


TOKEN = """\
pub struct Token {
    pub raw: String,
}

impl Token {
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("empty".into());
        }
        Ok(Self { raw: raw.to_string() })
    }
}
"""

TOKEN_VERIFY = """\
pub struct Token {
    pub raw: String,
    pub expires_at: u64,
}

impl Token {
    pub fn verify(raw: &str, now: u64) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("empty".into());
        }
        let expires_at = 9_999;
        if expires_at < now {
            return Err("expired".into());
        }
        Ok(Self {
            raw: raw.to_string(),
            expires_at,
        })
    }
}
"""

AUTH = """\
use crate::token::Token;

pub fn login(raw: &str) -> Result<String, String> {
    let token = Token::parse(raw)?;
    Ok(format!("session:{}", token.raw))
}
"""

AUTH_VERIFY = """\
use crate::token::Token;

pub fn login(raw: &str, now: u64) -> Result<String, String> {
    let token = Token::verify(raw, now)?;
    Ok(format!("session:{}", token.raw))
}

pub fn format_token(token: &Token) -> String {
    format!("tok#{}", token.raw.len())
}
"""

SESSION = """\
use crate::token::Token;

pub fn bind(raw: &str) -> Result<Token, String> {
    Token::parse(raw)
}
"""

SESSION_VERIFY = """\
use crate::token::Token;

pub fn bind(raw: &str, now: u64) -> Result<Token, String> {
    Token::verify(raw, now)
}
"""

TESTS = """\
use crate::auth;
use crate::session;

pub fn test_login_ok() {
    assert!(auth::login("abc").is_ok());
}

pub fn test_bind_ok() {
    assert!(session::bind("abc").is_ok());
}
"""

TESTS_VERIFY = """\
use crate::auth;
use crate::session;

pub fn test_login_ok() {
    assert!(auth::login("abc", 1).is_ok());
}

pub fn test_bind_ok() {
    assert!(session::bind("abc", 1).is_ok());
}

pub fn test_expired_is_string_error() {
    let err = crate::token::Token::verify("abc", 10_000).unwrap_err();
    let _: String = err;
}
"""

PROTOCOL = """\
pub enum Msg {
    Ping,
}

pub fn decode(bytes: &str) -> Result<Msg, String> {
    if bytes.contains("ping") {
        return Ok(Msg::Ping);
    }
    Err("unknown".into())
}

pub fn encode(msg: &Msg) -> String {
    match msg {
        Msg::Ping => "ping".into(),
    }
}
"""

PROTOCOL_V2 = """\
pub enum Msg {
    Ping,
    Hello(String),
}

pub fn decode_v1(bytes: &str) -> Result<Msg, String> {
    if bytes.contains("ping") {
        return Ok(Msg::Ping);
    }
    Err("unknown".into())
}

pub fn decode(bytes: &str) -> Result<Msg, String> {
    if bytes.contains("\\"v\\":2") {
        if let Some(rest) = bytes.split("hello:").nth(1) {
            return Ok(Msg::Hello(rest.trim_matches('"').to_string()));
        }
        return Err("bad v2".into());
    }
    decode_v1(bytes)
}

pub fn encode(msg: &Msg) -> String {
    match msg {
        Msg::Ping => "ping".into(),
        Msg::Hello(name) => format!("{{\\"v\\":2,\\"hello\\":\\"{name}\\"}}"),
    }
}
"""

STORE = """\
pub struct User {
    pub id: String,
}

pub fn lookup(id: &str) -> Result<User, String> {
    if id == "missing" {
        return Err("not found".into());
    }
    Ok(User { id: id.into() })
}

pub fn load_user(id: &str) -> Result<User, String> {
    lookup(id)?
}
"""

STORE_FALLBACK = """\
pub struct User {
    pub id: String,
}

pub fn lookup(id: &str) -> Result<User, String> {
    if id == "missing" {
        return Err("not found".into());
    }
    Ok(User { id: id.into() })
}

pub fn load_user(id: &str) -> Result<User, String> {
    match lookup(id) {
        Ok(user) => Ok(user),
        Err(_) => Ok(User { id: "anonymous".into() }),
    }
}

pub fn cached_load(id: &str) -> Result<User, String> {
    load_user(id)
}
"""

CONFIG = """\
pub struct Config {
    pub host: String,
    pub port: u16,
}

pub fn load_config() -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 8080,
    }
}
"""

CONFIG_JSON = '{ "host": "127.0.0.1", "port": 8080 }\n'

CONFIG_FINAL = """\
pub struct Config {
    pub host: String,
    pub port: u16,
}

pub fn load_config() -> Config {
    let host = std::env::var("APP_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("APP_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);
    Config { host, port }
}
"""

CONFIG_LEAK_SEED = """\
pub struct Config {
    pub host: String,
    pub port: u16,
}

// leftover trap: prefer JSON host/port keys forever
pub fn load_config() -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 8080,
    }
}
"""

CONFIG_LEAK_GOLD = """\
pub struct Config {
    pub host: String,
    pub port: u16,
}

pub fn load_config() -> Config {
    let host = std::env::var("APP_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("APP_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);
    Config { host, port }
}
"""

SWITCH_AUTH = """\
pub fn allow(user: &str) -> bool {
    user == "admin"
}
"""

SWITCH_AUTH_GOLD = """\
pub fn allow(user: &str) -> bool {
    user == "admin" || user == "operator"
}

pub fn rate_limit(_user: &str) -> u32 {
    30
}
"""

SWITCH_BILLING = """\
pub fn total(items: &[u32]) -> u32 {
    let mut sum = 0;
    for i in 0..items.len() {
        sum += items[i + 1];
    }
    sum
}
"""

SWITCH_BILLING_GOLD = """\
pub fn total(items: &[u32]) -> u32 {
    let mut sum = 0;
    for i in 0..items.len() {
        sum += items[i];
    }
    sum
}
"""

PARSE = """\
pub fn visit_all(items: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..items.len() {
        out.push(items[i + 1]);
    }
    out
}
"""

PARSE_GOLD = """\
pub fn visit_all(items: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..items.len() {
        out.push(items[i]);
    }
    out
}
"""

DUMP = """\
pub fn noisy_log() -> String {
    let mut out = String::new();
    for i in 0..400 {
        out.push_str(&format!("unrelated compiler chatter line {i}\\n"));
    }
    out.push_str("error[E0308]: mismatched types in visit_all\\n");
    out.push_str("error: index out of bounds at items[i + 1]\\n");
    out
}
"""


def hidden(path, pred, *needles, min=None):
    row = {"path": path, "pred": pred, "needles": list(needles)}
    if min is not None:
        row["min"] = min
    return row


def task(**kwargs):
    return kwargs


def main() -> None:
    if ROOT.exists():
        pass
    write(ROOT / "seeds" / "authbox" / "src" / "token.rs", TOKEN)
    write(ROOT / "seeds" / "authbox" / "src" / "auth.rs", AUTH)
    write(ROOT / "seeds" / "authbox" / "src" / "session.rs", SESSION)
    write(ROOT / "seeds" / "authbox" / "src" / "tests.rs", TESTS)
    write(ROOT / "seeds" / "protocol" / "src" / "protocol.rs", PROTOCOL)
    write(ROOT / "seeds" / "fallback" / "src" / "store.rs", STORE)
    write(ROOT / "seeds" / "config" / "src" / "config.rs", CONFIG)
    write(ROOT / "seeds" / "config" / "config.json", CONFIG_JSON)
    write(ROOT / "seeds" / "config_leak" / "src" / "config.rs", CONFIG_LEAK_SEED)
    write(ROOT / "seeds" / "config_leak" / "config.json", CONFIG_JSON)
    write(ROOT / "seeds" / "switch" / "src" / "auth.rs", SWITCH_AUTH)
    write(ROOT / "seeds" / "switch" / "src" / "billing.rs", SWITCH_BILLING)
    write(ROOT / "seeds" / "noise" / "src" / "parse.rs", PARSE)
    write(ROOT / "seeds" / "noise" / "src" / "dump.rs", DUMP)

    write(ROOT / "checks" / "wire_v1.py", """\
import pathlib, sys
root = pathlib.Path(sys.argv[1])
text = (root / "src" / "protocol.rs").read_text(encoding="utf-8")
ok = ("decode_v1" in text or "ping" in text) and ("v\\":2" in text or "Hello" in text)
sys.exit(0 if ok else 1)
""")
    write(ROOT / "checks" / "fallback_anonymous.py", """\
import pathlib, sys
root = pathlib.Path(sys.argv[1])
text = (root / "src" / "store.rs").read_text(encoding="utf-8")
sys.exit(0 if "anonymous" in text and "lookup(id)?" not in text else 1)
""")

    verify_hidden = [
        hidden("src/token.rs", "contains", "fn verify"),
        hidden("src/token.rs", "not_contains", "fn parse"),
        hidden("src/auth.rs", "contains", "Token::verify"),
        hidden("src/session.rs", "contains", "Token::verify"),
    ]
    long_hidden = verify_hidden + [
        hidden("src/token.rs", "contains", "expires_at"),
        hidden("src/tests.rs", "contains", "expired"),
        hidden("src/auth.rs", "contains", "format_token"),
    ]
    refactor_hidden = verify_hidden + [
        hidden("src/auth.rs", "contains", "now"),
        hidden("src/session.rs", "contains", "now"),
    ]
    relitigate_hidden = refactor_hidden + [
        hidden("src/auth.rs", "contains", "format_token"),
        hidden("src/token.rs", "contains", "expires_at"),
    ]

    short_ops = [
        {"op": "user", "text": (
            "In this small Rust crate, rename Token::parse to Token::verify and update every caller "
            "in src/auth.rs, src/session.rs, and src/tests.rs. Do not add extra features."
        )}
    ]
    long_ops = [
        {"op": "user", "text": "Rename Token::parse to Token::verify in src/token.rs only. Leave callers broken for now."},
        {"op": "user", "text": "Update src/auth.rs so login uses Token::verify."},
        {"op": "user", "text": "Update src/session.rs so bind uses Token::verify."},
        {"op": "user", "text": "Fix src/tests.rs to compile against verify."},
        {"op": "user", "text": "Add an expires_at: u64 field on Token. Existing tokens expire at 9999."},
        {"op": "user", "text": "verify must take now: u64 and return Err(\"expired\") when expires_at < now. Keep the error type as String."},
        {"op": "user", "text": "Thread now through auth::login and session::bind."},
        {"op": "user", "text": "Add a test_expired_is_string_error in src/tests.rs."},
        {"op": "user", "text": "Session must not persist an expired token."},
        {"op": "user", "text": "Add auth::format_token that logs only the token length, never the raw bytes."},
        {"op": "user", "text": "Do not log raw token bytes anywhere."},
        {"op": "user", "text": "Final pass: every caller compiles, expiry is a String error, format_token exists."},
    ]
    refactor_ops = [
        {"op": "user", "text": "Change the Token interface: parse becomes verify(raw, now). Update token.rs."},
        {"op": "user", "text": "Update every caller of Token::parse."},
        {"op": "user", "text": "The tests fail because they still call parse. Fix src/tests.rs."},
        {"op": "user", "text": "Add expires_at and reject expired tokens with a String error."},
        {"op": "user", "text": "Re-run the tests mentally and keep String errors. Do not switch to a custom error enum."},
    ]
    relitigate_ops = refactor_ops + [
        {"op": "user", "text": "New constraint: add format_token and never log raw token bytes."},
    ]

    tasks = [
        task(
            id="horizon_short", scenario="horizon", variant="short",
            name="Same auth rename, one user turn",
            seed="authbox", include_rolling=False,
            target_rounds_lo=3, target_rounds_hi=5,
            expected_edit="rename parse to verify and update callers",
            ops=short_ops, hidden=verify_hidden,
        ),
        task(
            id="horizon_long", scenario="horizon", variant="long",
            name="Same auth rename stretched across explore/test/edit turns",
            seed="authbox", include_rolling=True,
            target_rounds_lo=25, target_rounds_hi=40,
            expected_edit="rename parse to verify, expiry, format_token",
            ops=long_ops, hidden=long_hidden,
        ),
        task(
            id="long_refactor", scenario="refactor", variant="once",
            name="Interface change, callers, failing tests, then a constraint",
            seed="authbox", include_rolling=False,
            target_rounds_lo=15, target_rounds_hi=25,
            expected_edit="verify + now + String expiry error",
            ops=refactor_ops, hidden=refactor_hidden,
        ),
        task(
            id="long_refactor_relitigate", scenario="refactor", variant="relitigate",
            name="Same refactor plus a second constraint after the first lands",
            seed="authbox", include_rolling=False,
            target_rounds_lo=15, target_rounds_hi=25,
            expected_edit="verify + expiry + format_token",
            ops=relitigate_ops, hidden=relitigate_hidden,
        ),
        task(
            id="semantic_recall", scenario="semantic", variant="wire",
            name="Verbal wire-compat constraint, then a v2 change",
            seed="protocol", include_rolling=True,
            target_rounds_lo=10, target_rounds_hi=20,
            expected_edit="add v2 Hello while keeping unversioned ping",
            ops=[
                {"op": "user", "text": (
                    "Important constraint that must not be written into any file comments: "
                    "old clients send an unversioned ping body and must keep working. "
                    "Do not mention this constraint in source."
                )},
                {"op": "user", "text": "Read src/protocol.rs and summarize the current message types."},
                {"op": "user", "text": "Add a unit-style comment-free helper encode roundtrip for Ping."},
                {"op": "user", "text": "List possible v2 fields you might add. Do not add them yet."},
                {"op": "user", "text": "Sketch how a Hello message could look. Still no v2 in code."},
                {"op": "user", "text": "Add logging around decode errors without changing the grammar."},
                {"op": "user", "text": "Clean up any unused notes. Keep Ping working."},
                {"op": "user", "text": "Now add v2: Msg::Hello(String) with an explicit v:2 envelope."},
                {"op": "user", "text": "decode must still accept the original unversioned ping body."},
                {"op": "user", "text": "Final check: v2 Hello exists and unversioned ping still decodes."},
            ],
            hidden=[
                hidden("src/protocol.rs", "contains", "Hello"),
                hidden("src/protocol.rs", "contains", "ping"),
                hidden("src/protocol.rs", "contains", "decode_v1"),
            ],
            hidden_commands=[{"name": "wire_v1", "script": "wire_v1.py"}],
        ),
        task(
            id="semantic_recall_fallback", scenario="semantic", variant="fallback",
            name="Verbal fallback-not-propagate constraint, then cache work",
            seed="fallback", include_rolling=False,
            target_rounds_lo=10, target_rounds_hi=20,
            expected_edit="missing user returns anonymous, never lookup(id)?",
            ops=[
                {"op": "user", "text": (
                    "Constraint not to write into files: a missing user must fallback to an anonymous "
                    "user and must not propagate the lookup error."
                )},
                {"op": "user", "text": "Read src/store.rs."},
                {"op": "user", "text": "Add a cached_load wrapper that calls load_user."},
                {"op": "user", "text": "Document nothing. Just keep exploring load_user."},
                {"op": "user", "text": "Add a comment-free id normalizer that trims whitespace."},
                {"op": "user", "text": "Now implement the missing-user policy in load_user."},
                {"op": "user", "text": "cached_load must use the same fallback."},
                {"op": "user", "text": "Final: anonymous fallback, no lookup(id)? propagation."},
            ],
            hidden=[
                hidden("src/store.rs", "contains", "anonymous"),
                hidden("src/store.rs", "not_contains", "lookup(id)?"),
                hidden("src/store.rs", "contains", "cached_load"),
            ],
            hidden_commands=[{"name": "fallback", "script": "fallback_anonymous.py"}],
        ),
        task(
            id="supersession", scenario="supersession", variant="clean",
            name="JSON then TOML then env-first config",
            seed="config", include_rolling=False,
            target_rounds_lo=8, target_rounds_hi=16,
            expected_edit="env APP_HOST/APP_PORT override defaults",
            ops=[
                {"op": "user", "text": "Load config from JSON in config.json."},
                {"op": "user", "text": "Switch the on-disk format plan to TOML. You may keep defaults in code."},
                {"op": "user", "text": "TOML stays the file format, but environment variables must win."},
                {"op": "user", "text": "Implement load_config with APP_HOST and APP_PORT, defaulting to 127.0.0.1:8080."},
            ],
            hidden=[
                hidden("src/config.rs", "contains", "APP_HOST"),
                hidden("src/config.rs", "contains", "APP_PORT"),
                hidden("src/config.rs", "not_contains", "serde_json"),
            ],
        ),
        task(
            id="supersession_leak", scenario="supersession", variant="leak",
            name="Same config evolution with a leftover JSON trap comment",
            seed="config_leak", include_rolling=False,
            target_rounds_lo=8, target_rounds_hi=16,
            expected_edit="env wins; leftover JSON trap comment must not remain as policy",
            ops=[
                {"op": "user", "text": "The leftover comment about JSON keys is stale. Env must win."},
                {"op": "user", "text": "Implement APP_HOST / APP_PORT overrides."},
                {"op": "user", "text": "Delete the JSON-forever trap comment so it cannot leak as the latest decision."},
            ],
            hidden=[
                hidden("src/config.rs", "contains", "APP_HOST"),
                hidden("src/config.rs", "not_contains", "prefer JSON"),
            ],
        ),
        task(
            id="task_switch", scenario="task_switch", variant="short_b",
            name="Auth work, suspend for a billing fix, resume auth",
            seed="switch", include_rolling=True,
            target_rounds_lo=12, target_rounds_hi=24,
            expected_edit="auth rate_limit + operator allow; billing index fix",
            ops=[
                {"op": "user", "text": "Task A: in src/auth.rs allow operator as well as admin. Do not touch billing."},
                {"op": "user", "text": "Still on A: add rate_limit(user) -> 30."},
                {"op": "suspend"},
                {"op": "user", "text": "Task B: src/billing.rs total() indexes one past the end. Fix it. Do not change auth."},
                {"op": "user", "text": "On B: double-check the loop uses items[i]."},
                {"op": "activate", "slot": "first"},
                {"op": "user", "text": "Back on A: confirm operator is allowed and rate_limit is 30. Do not revert billing."},
            ],
            hidden=[
                hidden("src/auth.rs", "contains", "operator"),
                hidden("src/auth.rs", "contains", "rate_limit"),
                hidden("src/billing.rs", "contains", "items[i]"),
                hidden("src/billing.rs", "not_contains", "items[i + 1]"),
            ],
        ),
        task(
            id="task_switch_long_b", scenario="task_switch", variant="long_b",
            name="Same switch with a longer billing interrupt",
            seed="switch", include_rolling=False,
            target_rounds_lo=16, target_rounds_hi=28,
            expected_edit="auth rate_limit + operator; billing index fix after a long B",
            ops=[
                {"op": "user", "text": "Task A: allow operator in src/auth.rs and add rate_limit -> 30."},
                {"op": "suspend"},
                {"op": "user", "text": "Task B: inspect src/billing.rs. Do not fix yet."},
                {"op": "user", "text": "Task B: explain the off-by-one in words, still no edit."},
                {"op": "user", "text": "Task B: now fix items[i + 1] to items[i]."},
                {"op": "user", "text": "Task B: add a trivial comment-free second loop pass that also uses items[i]."},
                {"op": "user", "text": "Task B: remove that second pass if it duplicates work; keep the fix."},
                {"op": "activate", "slot": "first"},
                {"op": "user", "text": "Resume A: operator + rate_limit must still be present."},
            ],
            hidden=[
                hidden("src/auth.rs", "contains", "operator"),
                hidden("src/auth.rs", "contains", "rate_limit"),
                hidden("src/billing.rs", "not_contains", "items[i + 1]"),
            ],
        ),
        task(
            id="noise_recovery", scenario="noise", variant="once",
            name="Huge unrelated logs plus two real errors",
            seed="noise", include_rolling=False,
            target_rounds_lo=6, target_rounds_hi=16,
            expected_edit="fix items[i + 1] in visit_all",
            ops=[
                {"op": "user", "text": "Run through src/dump.rs noisy_log output in your head. Ignore unrelated chatter."},
                {"op": "user", "text": "There are two real errors about visit_all / items[i + 1]. Fix src/parse.rs."},
                {"op": "user", "text": "Do not delete dump.rs. Keep the noisy helper."},
            ],
            hidden=[
                hidden("src/parse.rs", "contains", "items[i]"),
                hidden("src/parse.rs", "not_contains", "items[i + 1]"),
                hidden("src/dump.rs", "contains", "unrelated compiler chatter"),
            ],
        ),
        task(
            id="noise_repeat_fail", scenario="noise", variant="repeat",
            name="Same bug with three repeated failing test dumps",
            seed="noise", include_rolling=False,
            target_rounds_lo=8, target_rounds_hi=18,
            expected_edit="fix visit_all after repeated noisy failures",
            ops=[
                {"op": "user", "text": "The tests dumped src/dump.rs. The real error is items[i + 1]. Do not fix yet; just read."},
                {"op": "user", "text": "Tests failed again with the same dump. Still the same two real errors."},
                {"op": "user", "text": "Third failure, same dump. Now fix visit_all."},
                {"op": "user", "text": "Keep dump.rs. Confirm parse.rs no longer uses items[i + 1]."},
            ],
            hidden=[
                hidden("src/parse.rs", "not_contains", "items[i + 1]"),
                hidden("src/dump.rs", "contains", "index out of bounds"),
            ],
        ),
    ]

    names = []
    for spec in tasks:
        names.append(f"tasks/{spec['id']}.json")
        write(ROOT / "tasks" / f"{spec['id']}.json", json.dumps(spec, indent=2) + "\n")

    write(ROOT / "pack.json", json.dumps({"schema": "agent-eval.context-bench.v1", "tasks": names}, indent=2) + "\n")

    def gold(task_id, mapping):
        for rel, body in mapping.items():
            write(ROOT / "golden" / task_id / rel, body)

    gold("horizon_short", {
        "src/token.rs": TOKEN.replace("fn parse", "fn verify").replace("Token::parse", "Token::verify"),
        "src/auth.rs": AUTH.replace("Token::parse", "Token::verify"),
        "src/session.rs": SESSION.replace("Token::parse", "Token::verify"),
        "src/tests.rs": TESTS.replace("parse", "verify") if False else TESTS.replace("login(\"abc\")", "login(\"abc\")"),
    })
    # horizon_short tests still call login/bind without now — verify_hidden doesn't require tests.rs changes.
    gold("horizon_short", {
        "src/token.rs": TOKEN.replace("pub fn parse", "pub fn verify"),
        "src/auth.rs": AUTH.replace("Token::parse", "Token::verify"),
        "src/session.rs": SESSION.replace("Token::parse", "Token::verify"),
    })
    gold("horizon_long", {
        "src/token.rs": TOKEN_VERIFY,
        "src/auth.rs": AUTH_VERIFY,
        "src/session.rs": SESSION_VERIFY,
        "src/tests.rs": TESTS_VERIFY,
    })
    gold("long_refactor", {
        "src/token.rs": TOKEN_VERIFY,
        "src/auth.rs": AUTH.replace("Token::parse(raw)", "Token::verify(raw, now)").replace(
            "pub fn login(raw: &str)", "pub fn login(raw: &str, now: u64)"
        ),
        "src/session.rs": SESSION.replace("Token::parse(raw)", "Token::verify(raw, now)").replace(
            "pub fn bind(raw: &str)", "pub fn bind(raw: &str, now: u64)"
        ),
        "src/tests.rs": TESTS.replace("login(\"abc\")", "login(\"abc\", 1)").replace("bind(\"abc\")", "bind(\"abc\", 1)"),
    })
    gold("long_refactor_relitigate", {
        "src/token.rs": TOKEN_VERIFY,
        "src/auth.rs": AUTH_VERIFY,
        "src/session.rs": SESSION_VERIFY,
        "src/tests.rs": TESTS_VERIFY,
    })
    gold("semantic_recall", {"src/protocol.rs": PROTOCOL_V2})
    gold("semantic_recall_fallback", {"src/store.rs": STORE_FALLBACK})
    gold("supersession", {"src/config.rs": CONFIG_FINAL})
    gold("supersession_leak", {"src/config.rs": CONFIG_LEAK_GOLD})
    gold("task_switch", {"src/auth.rs": SWITCH_AUTH_GOLD, "src/billing.rs": SWITCH_BILLING_GOLD})
    gold("task_switch_long_b", {"src/auth.rs": SWITCH_AUTH_GOLD, "src/billing.rs": SWITCH_BILLING_GOLD})
    gold("noise_recovery", {"src/parse.rs": PARSE_GOLD})
    gold("noise_repeat_fail", {"src/parse.rs": PARSE_GOLD})

    print("wrote", ROOT)


if __name__ == "__main__":
    main()
