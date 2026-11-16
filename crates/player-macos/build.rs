use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=KICK_PLAYER_MACOS_LIB_DIR");
    let library_dir = env::var_os("KICK_PLAYER_MACOS_LIB_DIR")
        .map(PathBuf::from)
        .expect(
            "set KICK_PLAYER_MACOS_LIB_DIR to the directory containing libKickPlayerCBridge.dylib",
        );

    println!("cargo:rustc-link-search=native={}", library_dir.display());
    println!("cargo:rustc-link-lib=dylib=KickPlayerCBridge");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", library_dir.display());
}
