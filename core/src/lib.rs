//! Shrinkr core: inspection, pipeline selection, FFmpeg backend,
//! benchmark matrix. UI-agnostic — shared by the Dioxus GUI and the
//! headless CLI.

pub mod bench;
pub mod convert;
pub mod ffmpeg;
pub mod hw;
pub mod images;
pub mod log;
pub mod media;
pub mod pipeline;
pub mod process;
pub mod target;
pub mod update;
