//! Crowsnest: a read-only terminal dashboard for a firstmate fleet.
//!
//! The crate is split so the data layer can be reused without the UI:
//!
//! - [`exec`] is the only place a program is started, from a read-only allowlist.
//! - [`sources`] turns each read into [`model`] types.
//! - [`collector`] runs the sources on background threads at their cadences.
//! - [`config`] reads and writes crowsnest's own settings file.
//! - [`app`], [`keys`], [`options`] and [`ui`] hold UI state, key handling
//!   and rendering, reading only [`model`].

pub mod app;
pub mod collector;
pub mod config;
pub mod exec;
pub mod keys;
pub mod model;
pub mod options;
pub mod sources;
pub mod timefmt;
pub mod toon;
pub mod ui;
