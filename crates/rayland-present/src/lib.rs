//! **`rayland-present`**: put a finished frame in a window on S's display.
//!
//! S is the machine the user is looking at — the one with the GPU, the monitor and the Wayland
//! compositor. This crate is the last step of everything Rayland does: however the pixels were
//! produced, and wherever the application that asked for them is running, they end up here and go
//! on screen.
//!
//! # Where this code came from, and why it moved
//! This is `rayland-server`'s `window.rs`, extracted whole by (c)1 Task 7. It was not extracted for
//! neatness — it was extracted because a **second** binary now needs the identical code:
//!
//! - **`rayland-server`** (arc (s), SP0–SP3) renders a triangle from Rayland's own hand-rolled
//!   `postcard` command stream and presents it.
//! - **`rayland-s`** (arc (c), (c)1) presents the readback blob of a *real, unmodified* Vulkan
//!   application whose commands crossed a network from another machine.
//!
//! Copying ~700 lines of Wayland plumbing into the second one would have rotted: the two copies
//! would drift, and the drift would show up as one of them mysteriously not painting.
//!
//! # The two paths, and the honest status of each
//! [`present`] chooses between them at runtime and always says which it chose:
//!
//! - **dmabuf (`zwp_linux_dmabuf_v1`) — zero-copy.** The producer exports GPU memory as a kernel
//!   handle and the compositor samples it directly. No CPU round-trip. This is SP3's headline
//!   property and the path a real presentation stack wants.
//! - **`wl_shm` — a CPU copy.** The pixels are copied into a shared-memory buffer. Slower, and
//!   universal.
//!
//! **(c)1's `rayland-s` can only use the second one, and that is a fact about the domain rather
//! than about this crate.** Spec §7.1: the host cannot see the application's `DEVICE_LOCAL` render
//! target at all — C0 Task 4b established it produces **no blob**, because it is created by Venus
//! commands *inside the command ring* and never appears in the engine's resource table. There is
//! nothing there to dmabuf-export. So (c)1 presents the app's **readback buffer** through `wl_shm`,
//! with a GPU→CPU round trip on S. SP3's dmabuf work is not wasted — `rayland-server` still uses
//! it, and it is what a real presentation path will use — but (c)1 cannot reach it. See
//! `rayland_s::present` for the full account and for the shortcut's expiry date.
//!
//! # Where the seam was cut, and what deliberately stayed behind
//! The awkward part of the extraction is that SP3's `present` did not take a finished frame — it
//! took a live Vulkan `Renderer`, because *which kind of frame to render* depends on what the
//! compositor advertises, which is only knowable from inside `present`. [`FrameSource`] is the
//! trait that preserves that structure without dragging Vulkan across the split: `present` makes
//! the decision it alone can make, then asks the source for the matching shape.
//!
//! So the split is **presentation vs. production**, and it lands as:
//!
//! - **Here:** the Wayland plumbing, the capability probe, the `wl_shm` buffer + swizzle, the
//!   dmabuf *protocol* handling, and the *descriptions* of the two frame shapes ([`RenderedFrame`],
//!   [`DmabufFrame`]).
//! - **Left in `rayland-server`:** every line of `ash`. The Vulkan code that *creates* a dmabuf
//!   export (`vkGetMemoryFdKHR`, `VK_EXT_image_drm_format_modifier`, the OPTIMAL→LINEAR blit whose
//!   channel-order reasoning is `dmabuf.rs`'s module docs) belongs with the renderer it serves. The
//!   test of whether a line belongs here is simple: **this crate must never need a GPU.** It is
//!   handed pixels, or handed an fd; it does not make either.
//!
//! `rayland_server::render` and `rayland_server::dmabuf` re-export the types that moved, so every
//! SP-era caller still names them exactly where it always did.

// The two shapes a presentable frame can take, plus the RGBA8 -> Xrgb8888 swizzle.
pub mod frame;
// The live Wayland window: the capability probe, the event loop, and the two draw paths.
pub mod window;

// Re-exported at the crate root so callers write `rayland_present::present` rather than
// `rayland_present::window::present`. The module split is an organizational detail of this crate;
// the flat surface is what its two consumers actually use.
pub use frame::{
    DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888, DmabufFrame, RenderedFrame, pack_xrgb8888,
};
pub use window::{FrameSource, WindowConfig, present};
