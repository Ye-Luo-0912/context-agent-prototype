pub enum Msg {
    Ping,
}

pub fn decode(bytes: &str) -> Result<Msg, String> {
    if bytes.contains("ping") {
        return Ok(Msg::Ping);
    }
    Err("unknown".into())
}

pub fn encode(msg: &Msg) -> String {
    match msg {
        Msg::Ping => "ping".into(),
    }
}
