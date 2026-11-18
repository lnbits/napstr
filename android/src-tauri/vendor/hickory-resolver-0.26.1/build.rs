fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios")
        && std::env::var_os("CARGO_FEATURE_SYSTEM_CONFIG").is_some()
    {
        cc::Build::new()
            .file("src/system_conf/ios.c")
            .compile("napstrfy_ios_dns");
        println!("cargo:rustc-link-lib=resolv");
        println!("cargo:rerun-if-changed=src/system_conf/ios.c");
    }
}
