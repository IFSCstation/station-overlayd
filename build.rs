fn main() {
    // Compiles ui/overlay.slint. With slint-build's default `renderer-software`
    // feature, glyphs of imported fonts are pre-rasterized into the generated
    // code, so the runtime needs no font loading at all.
    slint_build::compile("ui/overlay.slint").expect("failed to compile ui/overlay.slint");
}
