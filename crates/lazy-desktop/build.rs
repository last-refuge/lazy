//! Windows only: puts the product name, description, and version (and the icon, when
//! `branding/icons/lazy-app.ico` exists) into the executable. On every other host this does nothing
//! and the `winresource` build dependency is not even fetched. (Building for Windows from another
//! operating system does not embed the resource; build on Windows for a release.)

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Adding the icon later makes the next build pick it up.
    println!("cargo:rerun-if-changed=../../branding/icons/lazy-app.ico");
    embed_windows_resource();
}

#[cfg(windows)]
fn embed_windows_resource() {
    use std::path::PathBuf;

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    // The file and product versions come from this crate's version.
    resource.set("ProductName", "Lazy");
    resource.set("FileDescription", "Lazy");
    resource.set(
        "Comments",
        "A native place for channels, messages and the people you work with.",
    );
    let icon = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("../../branding/icons/lazy-app.ico");
    if icon.exists() {
        if let Some(icon) = icon.to_str() {
            resource.set_icon(icon);
        }
    }
    // The program works without it: say so and carry on.
    if let Err(error) = resource.compile() {
        println!("cargo:warning=could not embed the Windows resource: {error}");
    }
}

#[cfg(not(windows))]
fn embed_windows_resource() {}
