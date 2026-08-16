pub struct User {
    pub id: String,
}

pub fn lookup(id: &str) -> Result<User, String> {
    if id == "missing" {
        return Err("not found".into());
    }
    Ok(User { id: id.into() })
}

pub fn load_user(id: &str) -> Result<User, String> {
    lookup(id)?
}
