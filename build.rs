fn main() {
    // Compile the ui/ project (slint-project.json lists overlay, menu, notify).
    // With slint-build's default `renderer-software` feature, glyphs of
    // imported fonts are pre-rasterized into the generated code, so the
    // runtime needs no font loading at all.
    slint_build::compile("ui/").expect("failed to compile ui/*.slint");
}
