pub fn noisy_log() -> String {
    let mut out = String::new();
    for i in 0..400 {
        out.push_str(&format!("unrelated compiler chatter line {i}\n"));
    }
    out.push_str("error[E0308]: mismatched types in visit_all\n");
    out.push_str("error: index out of bounds at items[i + 1]\n");
    out
}
