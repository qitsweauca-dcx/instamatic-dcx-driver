//! dectris_common depends on `zmq` (for `DriverError::Zmq(#[from] zmq::Error)`, DESIGN §6), so its own
//! test binaries link libzmq too. libzmq (zmq-sys 0.12, built from source) calls advapi32 from
//! ip.cpp on MSVC (DESIGN §4.3). `rustc-link-lib` propagates to every dependent crate.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-lib=advapi32");
    }
}
