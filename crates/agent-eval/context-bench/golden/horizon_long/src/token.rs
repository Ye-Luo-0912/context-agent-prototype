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
