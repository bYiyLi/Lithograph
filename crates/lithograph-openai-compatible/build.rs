fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // libsqlite3-sys may discover SQLite through pkg-config even in
        // loadable-extension mode. The generated bindings use only the host
        // API table, so discard that unused dylib load command.
        println!("cargo:rustc-link-arg=-Wl,-dead_strip_dylibs");
    }
}
