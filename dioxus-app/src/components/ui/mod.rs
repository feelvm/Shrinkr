//! Vendored rust-ui (Dioxus) components, shadcn-style.
//! Source: https://github.com/rust-ui/dioxus-ui (MIT).
//! Kept as plain files (no registry dependency) so the app builds
//! offline and stays on Dioxus 0.7.
//!
//! Only the pieces the app uses are re-exported; the rest stay
//! available for future screens.

#![allow(unused_imports)]

pub mod alert;
pub mod badge;
pub mod button;
pub mod callout;
pub mod card;
pub mod checkbox;
pub mod empty;
pub mod label;
pub mod progress;
pub mod separator;
pub mod slider;
pub mod spinner;
pub mod status;
pub mod switch;
pub mod table;
pub mod tabs;

pub use alert::*;
pub use badge::*;
pub use button::*;
pub use callout::*;
pub use card::*;
pub use checkbox::*;
pub use empty::*;
pub use label::*;
pub use progress::*;
pub use separator::*;
pub use slider::*;
pub use spinner::*;
pub use status::*;
pub use switch::*;
pub use table::*;
pub use tabs::*;
