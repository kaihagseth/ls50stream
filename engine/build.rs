fn main() {
    println!("cargo:rerun-if-changed=src/xpc_shim.c");
    println!("cargo:rerun-if-changed=src/output_shim.c");
    cc::Build::new().file("src/xpc_shim.c").flag("-fblocks").compile("ls50stream_xpc_shim");
    cc::Build::new().file("src/output_shim.c").compile("ls50stream_output_shim");
    println!("cargo:rustc-link-lib=framework=CoreAudio");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
}
