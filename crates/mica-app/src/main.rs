fn main() {
    #[cfg(windows)]
    std::process::exit(mica_app::handle_cli());
    #[cfg(not(windows))]
    eprintln!("mica-app only runs on Windows; this is a stub build.");
}
