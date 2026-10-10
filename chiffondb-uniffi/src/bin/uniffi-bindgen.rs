// Bindgen entry point. The Gradle plugin invokes this binary to generate the
// Kotlin Multiplatform bindings from the exported metadata.
fn main() {
    uniffi::uniffi_bindgen_main()
}
