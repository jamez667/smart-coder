//! Keyboard and gamepad polling, mapped to ship commands.

/// Read the current input state and turn it into thrust and turn commands.
/// Held keys accumulate, so tapping and holding feel the same at low speed.
pub fn poll_input(state: &mut ShipState, keys: &KeyState) {
    state.thrust = if keys.down(Key::W) { 1.0 } else { 0.0 };
    state.turn = keys.axis(Key::A, Key::D);
}
