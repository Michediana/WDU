fn main() {
    // The exe icon, for Explorer and the taskbar.
    embed_resource::compile("wdu-ui.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
