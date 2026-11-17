fn main() {
    // Иконка exe и сведения о версии (видны в проводнике, в «Пуске» и в установщике).
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets");
    embed_resource::compile("assets/app.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
