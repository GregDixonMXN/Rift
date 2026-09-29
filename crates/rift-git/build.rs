//! rift-git links libgit2, which calls into advapi32 (registry, SIDs,
//! CryptoAPI) on Windows. libgit2-sys doesn't always emit that link,
//! so declare it here where the dependency is owned.

fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        println!("cargo:rustc-link-lib=advapi32");
    }
}
