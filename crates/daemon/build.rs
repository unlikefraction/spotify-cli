//! Embeds `Info.plist` in the macOS `spotify-daemon` executable.
//!
//! A bare executable has no bundle, so the linker puts the plist in the `__TEXT,__info_plist`
//! section, where macOS and `codesign` look for it: the signature binds it, and the Automation
//! prompt shows its `NSAppleEventsUsageDescription`. `@VERSION@` becomes the crate's
//! `MAJOR.MINOR.PATCH` (the format `CFBundleShortVersionString` allows). Other targets (Linux,
//! Windows) link unchanged.

use std::error::Error;
use std::path::PathBuf;
use std::{env, fs};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Info.plist");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return Ok(());
    }
    let version = format!(
        "{}.{}.{}",
        env::var("CARGO_PKG_VERSION_MAJOR")?,
        env::var("CARGO_PKG_VERSION_MINOR")?,
        env::var("CARGO_PKG_VERSION_PATCH")?
    );
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("no CARGO_MANIFEST_DIR")?);
    let template = fs::read_to_string(manifest_dir.join("Info.plist"))?;
    if !template.contains("@VERSION@") {
        return Err("crates/daemon/Info.plist lost its @VERSION@ placeholder".into());
    }
    let plist = PathBuf::from(env::var_os("OUT_DIR").ok_or("no OUT_DIR")?).join("Info.plist");
    fs::write(&plist, template.replace("@VERSION@", &version))?;
    let path = plist.to_str().ok_or("OUT_DIR is not UTF-8")?;
    if path.contains(',') {
        // -Wl splits its argument at commas.
        return Err(format!("cannot pass {path} to the linker: it contains a comma").into());
    }
    println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{path}");
    Ok(())
}
