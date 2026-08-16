use crate::token::Token;

pub fn login(raw: &str) -> Result<String, String> {
    let token = Token::parse(raw)?;
    Ok(format!("session:{}", token.raw))
}
