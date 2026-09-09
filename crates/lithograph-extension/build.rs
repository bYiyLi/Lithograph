fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // libsqlite3-sys probes pkg-config even in loadable-extension mode. On
        // macOS hosts where sqlite3.pc is discoverable that can add an unused
        // -lsqlite3 to the final dylib. Lithograph calls SQLite exclusively via
        // the host API table, so strip that unused private runtime dependency.
        println!("cargo::rustc-link-arg-cdylib=-Wl,-dead_strip_dylibs");
    }
}
