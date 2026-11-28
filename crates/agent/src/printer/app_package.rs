// SPDX-License-Identifier: GPL-3.0-only

//! `install` / `uninstall`: the Print Support App package
//! (`ameleo-display-agent-print.msix`, next to the executable; the app is
//! a separate program).
//!
//! - Its signing certificate (`ameleo-display-agent-print.cer`, next to it) is
//!   trusted: `LocalMachine\TrustedPeople`.
//! - The package is staged and provisioned for all users (each one gets it
//!   at their next logon), and added for the user running `install` (at
//!   once; an older version is updated).
//! - `uninstall` deprovisions it and removes it for all users: their
//!   virtual printers go with it.

use std::path::Path;

use anyhow::{Context, Result, bail};
use proto::print_link::PACKAGE_FAMILY_NAME;
use windows::Foundation::Uri;
use windows::Management::Deployment::{
    DeploymentOptions, DeploymentResult, PackageManager, RemovalOptions,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows_collections::IIterable;
use windows_core::HSTRING;
use windows_sys::Win32::Security::Cryptography::{
    CERT_STORE_ADD_REPLACE_EXISTING, CERT_STORE_PROV_SYSTEM_W, CERT_SYSTEM_STORE_LOCAL_MACHINE,
    CertAddCertificateContextToStore, CertCloseStore, CertCreateCertificateContext,
    CertFreeCertificateContext, CertOpenStore, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};

use crate::win::wide;

pub const MSIX: &str = "ameleo-display-agent-print.msix";
const CERTIFICATE: &str = "ameleo-display-agent-print.cer";

fn deployed(what: &str, r: windows_core::Result<DeploymentResult>) -> Result<()> {
    let r = r.with_context(|| what.to_string())?;
    let code = r.ExtendedErrorCode()?;
    if code.is_err() {
        bail!("{what}: {} ({:#010x})", r.ErrorText()?, code.0);
    }
    Ok(())
}

/// Adds the certificate (DER) to `LocalMachine\TrustedPeople`.
fn trust(cer: &Path) -> Result<()> {
    let der = std::fs::read(cer).with_context(|| cer.display().to_string())?;
    unsafe {
        let ctx = CertCreateCertificateContext(
            X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
            der.as_ptr(),
            der.len() as u32,
        );
        if ctx.is_null() {
            bail!(
                "{}: not a certificate: {}",
                cer.display(),
                std::io::Error::last_os_error()
            );
        }
        let store = CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W,
            0,
            0,
            CERT_SYSTEM_STORE_LOCAL_MACHINE,
            wide("TrustedPeople").as_ptr().cast(),
        );
        if store.is_null() {
            let e = std::io::Error::last_os_error();
            CertFreeCertificateContext(ctx);
            bail!("LocalMachine\\TrustedPeople: {e}");
        }
        let ok = CertAddCertificateContextToStore(
            store,
            ctx,
            CERT_STORE_ADD_REPLACE_EXISTING,
            std::ptr::null_mut(),
        );
        let e = std::io::Error::last_os_error();
        CertFreeCertificateContext(ctx);
        CertCloseStore(store, 0);
        if ok == 0 {
            bail!("trusting {}: {e}", cer.display());
        }
    }
    Ok(())
}

/// `install`: the package next to the executable, if there.
pub fn install() -> Result<Option<std::path::PathBuf>> {
    let msix = std::env::current_exe()?.with_file_name(MSIX);
    if !msix.exists() {
        return Ok(None);
    }
    let cer = msix.with_file_name(CERTIFICATE);
    if cer.exists() {
        trust(&cer)?;
    }
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let uri = Uri::CreateUri(&HSTRING::from(format!(
        "file:///{}",
        msix.display().to_string().replace('\\', "/")
    )))?;
    let pm = PackageManager::new()?;
    let none = None::<&IIterable<Uri>>;
    deployed(
        "staging the package",
        pm.StagePackageAsync(&uri, none).and_then(|op| op.join()),
    )?;
    deployed(
        "provisioning the package for all users",
        pm.ProvisionPackageForAllUsersAsync(&HSTRING::from(PACKAGE_FAMILY_NAME))
            .and_then(|op| op.join()),
    )?;
    deployed(
        "adding the package for this user",
        pm.AddPackageAsync(&uri, none, DeploymentOptions::ForceApplicationShutdown)
            .and_then(|op| op.join()),
    )?;
    Ok(Some(msix))
}

/// `uninstall`: the package for all users, if installed.
pub fn uninstall() -> Result<bool> {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let pm = PackageManager::new()?;
    let family = HSTRING::from(PACKAGE_FAMILY_NAME);
    let packages: Vec<_> = pm
        .FindPackagesByPackageFamilyName(&family)?
        .into_iter()
        .collect();
    // Not provisioned: nothing to undo.
    let _ = pm
        .DeprovisionPackageForAllUsersAsync(&family)
        .and_then(|op| op.join());
    for p in &packages {
        let name = p.Id()?.FullName()?;
        deployed(
            &format!("removing {name}"),
            pm.RemovePackageWithOptionsAsync(&name, RemovalOptions::RemoveForAllUsers)
                .and_then(|op| op.join()),
        )?;
    }
    Ok(!packages.is_empty())
}
