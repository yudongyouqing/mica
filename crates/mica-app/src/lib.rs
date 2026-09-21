//! Windows shell. All interesting code lives in `app`; non-Windows builds are
//! empty because the windowing APIs do not exist there.

#[cfg(windows)]
mod app;

#[cfg(windows)]
pub fn run() {
    app::run();
}

#[cfg(not(windows))]
pub fn run() {
    eprintln!("mica-app only runs on Windows.");
}
