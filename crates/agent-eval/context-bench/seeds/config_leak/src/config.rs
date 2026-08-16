pub struct Config {
    pub host: String,
    pub port: u16,
}

// leftover trap: prefer JSON host/port keys forever
pub fn load_config() -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 8080,
    }
}
