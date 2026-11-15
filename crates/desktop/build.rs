fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/pi-qq.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/app.rc", embed_resource::NONE)
            .manifest_required()
            .expect("无法编译 Windows 应用图标资源");
    }
}
