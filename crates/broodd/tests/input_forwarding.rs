use broodd::should_forward_managed_pointer_button;
use smithay::backend::input::ButtonState;

#[test]
fn managed_pointer_button_forwards_press_only_with_target() {
    assert!(should_forward_managed_pointer_button(
        true,
        ButtonState::Pressed
    ));
    assert!(!should_forward_managed_pointer_button(
        false,
        ButtonState::Pressed
    ));
}

#[test]
fn managed_pointer_button_always_forwards_release() {
    assert!(should_forward_managed_pointer_button(
        true,
        ButtonState::Released
    ));
    assert!(should_forward_managed_pointer_button(
        false,
        ButtonState::Released
    ));
}
