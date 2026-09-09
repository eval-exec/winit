use super::*;

#[test]
fn enter_remains_named_when_android_character_map_returns_newline() {
    for code in [Keycode::Enter, Keycode::NumpadEnter] {
        assert_eq!(to_logical(Some(KeyMapChar::Unicode('\n')), code), Key::Named(NamedKey::Enter));
    }
}

#[test]
fn ctrl_j_is_not_rewritten_to_enter() {
    assert_eq!(
        to_logical(Some(KeyMapChar::Unicode('\n')), Keycode::J),
        Key::Character("\n".into())
    );
}

#[test]
fn modifier_sample_preserves_ctrl_and_shift_from_android() {
    let state = android_activity::input::MetaState(0x1001);
    let modifiers = modifiers_from_android(state);
    assert!(modifiers.control_key());
    assert!(modifiers.shift_key());
    assert!(!modifiers.alt_key());
    assert!(!modifiers.meta_key());
}
