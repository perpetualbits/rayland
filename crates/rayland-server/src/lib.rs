//! The Rayland server (library half).
//!
//! This crate's job in SP0 is to take a stream of [`rayland_wire::Message`] commands and
//! replay them on a real GPU, producing pixels. The GPU work lives in [`render`]; the
//! stream-handling that drives it is added in Task 6. Keeping this logic in a library
//! (rather than only in `main.rs`) is what lets the end-to-end test in Task 7 exercise it.

// The off-screen Vulkan renderer (this task).
pub mod render;
