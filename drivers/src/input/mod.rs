mod mouse;
mod ps2_input;

pub mod input_event_codes;

pub use mouse::{Mouse, MouseFlags, MouseState};
pub use ps2_input::Ps2Input;
