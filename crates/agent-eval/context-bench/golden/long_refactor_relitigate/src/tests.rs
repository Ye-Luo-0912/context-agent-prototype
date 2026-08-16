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
