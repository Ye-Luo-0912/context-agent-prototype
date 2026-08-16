pub enum Msg {
    Ping,
    Hello(String),
}

pub fn decode_v1(bytes: &str) -> Result<Msg, String> {
    if bytes.contains("ping") {
        return Ok(Msg::Ping);
    }
    Err("unknown".into())
}

pub fn decode(bytes: &str) -> Result<Msg, String> {
    if bytes.contains("\"v\":2") {
        if let Some(rest) = bytes.split("hello:").nth(1) {
            return Ok(Msg::Hello(rest.trim_matches('"').to_string()));
        }
        return Err("bad v2".into());
    }
    decode_v1(bytes)
}

pub fn encode(msg: &Msg) -> String {
    match msg {
        Msg::Ping => "ping".into(),
        Msg::Hello(name) => format!("{{\"v\":2,\"hello\":\"{name}\"}}"),
    }
}
