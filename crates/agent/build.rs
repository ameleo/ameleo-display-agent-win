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
    }
}
