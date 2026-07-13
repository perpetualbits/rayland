//! # Rayland — native remote GPU rendering for Wayland
//!
//! **This crate is currently an early-stage placeholder that exists to hold the
//! `rayland` name on crates.io while the project is designed and built.** It has no
//! usable functionality yet. Follow the repository for progress:
//! <https://github.com/perpetualbits/rayland>.
//!
//! ## What Rayland is (for the newcomer)
//!
//! Rayland lets you run a graphical application on one machine but have it *rendered
//! and displayed* on a *different* machine — the one that actually has the powerful
//! GPU and the monitor in front of you.
//!
//! It uses X11-era vocabulary, which is the opposite of modern "cloud" usage, so it
//! is worth stating carefully:
//!
//! * **S ("server" side)** — where *you sit*: the keyboard, mouse, monitor, GPU, the
//!   Wayland compositor, and working graphics drivers. Think: your capable laptop.
//! * **C ("client" side)** — where the *application program* runs. This might be a
//!   tiny, weak, or different-architecture computer (for example a RISC-V single-board
//!   computer), or a big headless server with lots of CPUs but no useful display path.
//!
//! The application on **C** draws by sending a *command stream* (the language of
//! rendering — "draw these triangles with this shader") across the network to **S**,
//! where **S's** GPU does the actual drawing and shows the result on **S's** screen.
//!
//! Crucially, Rayland ships *commands*, not *pixels*, in its primary mode. This is the
//! modern successor to the old X11 idea of network-transparent graphics — but for
//! Vulkan and modern OpenGL instead of the obsolete fixed-function pipeline. Shipping a
//! video stream of already-drawn pixels is supported as a *fallback*, not the goal,
//! because in Rayland's target setup the weak machine (C) is exactly the wrong place to
//! be doing expensive video encoding.
//!
//! The design — including why Wayland deliberately made this hard, and what pieces of
//! the ecosystem must grow to make it easy — lives in the repository under
//! `docs/design/`.

// Once real functionality lands, this file will most likely become the umbrella/facade
// of a Cargo workspace. For now it intentionally contains no code: a placeholder crate
// should compile cleanly and do nothing, so that publishing it makes no promises we
// have not yet kept.
