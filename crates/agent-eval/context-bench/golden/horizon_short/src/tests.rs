use crate::auth;
use crate::session;

pub fn test_login_ok() {
    assert!(auth::login("abc").is_ok());
}

pub fn test_bind_ok() {
    assert!(session::bind("abc").is_ok());
}
