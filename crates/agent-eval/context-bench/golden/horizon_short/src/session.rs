use crate::token::Token;

pub fn bind(raw: &str) -> Result<Token, String> {
    Token::verify(raw)
}
