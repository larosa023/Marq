fn main() {
    println!("cargo:rerun-if-changed=assets/marq.rc");
    println!("cargo:rerun-if-changed=assets/marq.ico");
    println!("cargo:rerun-if-changed=assets/marq.manifest");

    // Only embed the icon/manifest when actually building for Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/marq.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
