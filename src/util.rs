use std::time::SystemTime;

use ropey::RopeSlice;

/// Get the current time as an usize
pub fn time() -> usize {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as usize
}

/// Efficient way to test if a rope starts with the specified text.
pub fn rope_starts_with(slice: &RopeSlice, text: &str) -> bool {
    if slice.len_chars() < text.chars().count() {
        return false;
    }
    slice.chars().zip(text.chars()).all(|(x, y)| x == y)
}
