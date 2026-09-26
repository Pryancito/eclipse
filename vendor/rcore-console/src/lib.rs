//! The virtual console embedded in rCore kernel.
//!
//! The [`Console`] can be built on top of either a [`TextBuffer`] or a frame buffer ([`DrawTarget`]).
//!
//! This crate is no_std compatible.
//!
//! It can be tested in SDL2 with the help of [`embedded_graphics_simulator`](https://docs.rs//embedded-graphics/#simulator) crate.
//! See examples for details.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]
#![allow(warnings)]
#![allow(missing_docs)]

#[macro_use]
extern crate alloc;

#[cfg(feature = "log")]
#[macro_use]
extern crate log;
#[cfg(not(feature = "log"))]
#[macro_use]
mod log;

pub use cell::{Cell, Flags};
pub use console::{Console, ConsoleOnGraphic};
pub use embedded_graphics::{
    self,
    pixelcolor::Rgb888,
    prelude::{DrawTarget, OriginDimensions, Pixel, Size},
};
pub use graphic::TextOnGraphic;
// `Cell`'s `fg`/`bg` fields are public but their type was not, so nothing
// outside this crate could build a coloured cell -- or name the colour of
// one it read back.
pub use color::{Color, NamedColor};
pub use text_buffer::TextBuffer;
pub use text_buffer_cache::TextBufferCache;

mod ansi;
mod cell;
mod color;
mod console;
mod graphic;
mod text_buffer;
mod text_buffer_cache;

#[cfg(test)]
mod tests;
