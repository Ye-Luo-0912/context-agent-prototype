use crate::token::Token;

pub fn bind(raw: &str, now: u64) -> Result<Token, String> {
    Token::verify(raw, now)
}
