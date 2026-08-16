pub struct Config {
    pub host: String,
    pub port: u16,
}

pub fn load_config() -> Config {
    let host = std::env::var("APP_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("APP_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);
    Config { host, port }
}
