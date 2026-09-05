//! The index, and what is true about it.
//!
//! Everything in here is front-end-free by construction: no ratatui, no ANSI, no
//! `std::process::Command`, no `current_exe()`, nothing that reads a terminal.
//! That is not tidiness for its own sake — it is what lets one body of code be an
//! rlib inside the command-line tool and a static library inside something else,
//! without the two answering "what is new?" or "who is this author?" differently.
//!
//! The boundary is drawn where it is because `theme.rs` is ratatui and `render.rs`
//! imports it, so both stay with the binary. The half of `render.rs` with no colour
//! in it — wrapping, byline shortening, dates — is [`text`].
//!
//! `main.rs` keeps everything that is a *decision about a terminal*: the pager, the
//! opener, the detached refresh child, clap, and the panic hook that turns a closed
//! pipe into a clean exit. A library that called `process::exit` would be a hazard
//! to anything embedding it.

pub mod bib;
pub mod config;
pub mod dates;
pub mod db;
pub mod feed;
pub mod harvest;
pub mod names;
pub mod text;
pub mod venue;
