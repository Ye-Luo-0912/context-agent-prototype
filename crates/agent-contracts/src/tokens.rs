//! Shared token-estimation convention.
//!
//! The context engines, the runtime's prompt assembler and the replay
//! harness all measure the same quantity, so budget checks and the A/B/C
//! comparison stay comparable across implementations.

/// Estimate the token cost of a text: every four ASCII characters cost one
/// token, and every non-ASCII, non-whitespace character costs one token.
pub fn approx_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else if !ch.is_whitespace() {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4) + non_ascii
}
