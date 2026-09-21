fn main() {
    #[cfg(windows)]
    mica_app::run();
    #[cfg(not(windows))]
    eprintln!("mica-app only runs on Windows; this is a stub build.");
}
