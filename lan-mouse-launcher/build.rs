fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_resource::compile("../build-aux/lan-mouse.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
