fn main() {
    // Unoptimized debug builds overflow the default 1 MB Windows main-thread stack.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
}
