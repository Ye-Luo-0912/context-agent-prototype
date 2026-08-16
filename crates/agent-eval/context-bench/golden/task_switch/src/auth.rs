pub fn allow(user: &str) -> bool {
    user == "admin" || user == "operator"
}

pub fn rate_limit(_user: &str) -> u32 {
    30
}
