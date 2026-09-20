fn main() {
    #[cfg(windows)]
    wg_app::run();
    #[cfg(not(windows))]
    eprintln!("wg-app only runs on Windows; this is a stub build.");
}
