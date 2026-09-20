//! Terminal core: parser + screen state machine + pty + input encoding.
//! Pure logic; testable on any OS. Modules land in Tasks 2-4.

pub mod pty;
pub mod surface;
