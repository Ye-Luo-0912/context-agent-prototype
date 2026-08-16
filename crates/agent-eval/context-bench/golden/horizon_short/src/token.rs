pub struct Token {
    pub raw: String,
}

impl Token {
    pub fn verify(raw: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("empty".into());
        }
        Ok(Self { raw: raw.to_string() })
    }
}
