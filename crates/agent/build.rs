// SPDX-License-Identifier: GPL-3.0-only

//! Media Foundation is only loaded when the client cameras are used, so that
//! the agent starts without it: Windows 10 has no `MFCreateVirtualCamera`,
//! and the N editions of Windows have no Media Foundation without the Media
//! Feature Pack. The camera code checks that it is there first.

fn main() {
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if windows && msvc {
        for dll in ["mfplat.dll", "mf.dll", "mfsensorgroup.dll"] {
            println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg=delayimp.lib");
        // The application manifest (Common Controls 6, DPI awareness),
        // embedded in the executable and in the test binaries alike.
        let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("ameleo-display-agent.manifest");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }
}
