//! One text source for defaults, including const projections needed by render.
pub const SOURCE: &str = include_str!("../../config/defaults");
include!(concat!(env!("OUT_DIR"), "/config_defaults.rs"));
