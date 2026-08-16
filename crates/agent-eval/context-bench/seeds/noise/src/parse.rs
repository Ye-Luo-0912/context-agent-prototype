pub fn visit_all(items: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..items.len() {
        out.push(items[i + 1]);
    }
    out
}
