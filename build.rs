//! Records the target triple this binary was built for.
//!
//! `update check` needs it to pick its artifact out of a release manifest that
//! may name several. It has to be a *build-time* fact: deriving a triple from
//! the running system at runtime would let a node ask for, verify and install
//! a binary it cannot execute, and the failure would arrive as a service that
//! will not start rather than as a refused update.
//!
//! Cargo sets `TARGET` for build scripts and nowhere else, which is why this
//! file exists at all.

fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=DANSO_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
