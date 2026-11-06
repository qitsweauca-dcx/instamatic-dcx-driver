//! libzmq (zmq-sys 0.12, built from source) calls advapi32 from ip.cpp on MSVC (DESIGN §4.3).
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-lib=advapi32");
    }
}
