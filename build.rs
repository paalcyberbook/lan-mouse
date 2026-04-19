use shadow_rs::ShadowBuilder;

fn main() {
    ShadowBuilder::builder()
        .deny_const(Default::default())
        .build()
        .expect("shadow build");

    // Embed the Windows icon into the exe when targeting Windows.
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_resource::compile("build-aux/lan-mouse.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
