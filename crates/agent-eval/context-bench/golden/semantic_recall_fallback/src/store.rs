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
    match lookup(id) {
        Ok(user) => Ok(user),
        Err(_) => Ok(User { id: "anonymous".into() }),
    }
}

pub fn cached_load(id: &str) -> Result<User, String> {
    load_user(id)
}
