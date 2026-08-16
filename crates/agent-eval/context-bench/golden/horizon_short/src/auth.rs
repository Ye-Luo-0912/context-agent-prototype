use crate::token::Token;

pub fn login(raw: &str) -> Result<String, String> {
    let token = Token::verify(raw)?;
    Ok(format!("session:{}", token.raw))
}
