//! Crowsnest: a read-only terminal dashboard for a firstmate fleet.
//!
//! The crate is split so the data layer can be reused without the UI:
//!
//! - [`exec`] is the only place a program is started, from a read-only allowlist.
//! - [`sources`] turns each read into [`model`] types.
//! - [`lanes`] derives each runner's lane and verdict from a snapshot.
//! - [`collector`] runs the sources on background threads at their cadences.
//! - [`app`] and [`ui`] hold UI state and rendering, reading only [`model`].

pub mod app;
pub mod collector;
pub mod exec;
pub mod lanes;
pub mod model;
pub mod sources;
pub mod timefmt;
pub mod toon;
pub mod ui;
