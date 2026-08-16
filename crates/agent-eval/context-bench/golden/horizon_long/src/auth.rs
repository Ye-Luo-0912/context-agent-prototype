use crate::token::Token;

pub fn login(raw: &str, now: u64) -> Result<String, String> {
    let token = Token::verify(raw, now)?;
    Ok(format!("session:{}", token.raw))
}

pub fn format_token(token: &Token) -> String {
    format!("tok#{}", token.raw.len())
}
