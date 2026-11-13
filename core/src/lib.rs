//! Shrinkr core: inspection, pipeline selection, FFmpeg backend,
//! benchmark matrix, PDF shrinking, lossless image optimization.
//! UI-agnostic - shared by the Dioxus GUI and the headless CLI.

pub mod bench;
pub mod calib;
pub mod convert;
pub mod ffmpeg;
pub mod ffmpeg_fetch;
pub mod grain;
pub mod hw;
pub mod images;
pub mod log;
pub mod media;
pub mod optimize;
pub mod pause;
pub mod pdf;
pub mod pipeline;
pub mod process;
pub mod quality;
pub mod target;
pub mod update;
