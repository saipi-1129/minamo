use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=KICK_PLAYER_MACOS_LIB_DIR");
    let directory = PathBuf::from(
        env::var_os("KICK_PLAYER_MACOS_LIB_DIR")
            .expect("set KICK_PLAYER_MACOS_LIB_DIR to the SwiftPM product directory"),
    );
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", directory.display());
}
