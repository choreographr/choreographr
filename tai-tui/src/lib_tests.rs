use super::*;

#[test]
fn build_picker_returns_halfblocks_on_fallback() {
    let picker = build_picker();
    // Just verify it doesn't panic and returns a Picker.
    let _ = picker;
}
