fn main() {
    println!("cargo:rerun-if-changed=touchcursor.rc");
    println!("cargo:rerun-if-changed=static/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let root = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory is unavailable"),
    );
    embed_resource::compile(
        root.join("touchcursor.rc"),
        embed_resource::ParamsIncludeDirs([root]),
    )
    .manifest_required()
    .expect("failed to embed TouchCursor's Windows icon");
}
