//! Embarque viherd.exe et viherd-app.exe (déjà compilés en release) dans l'installeur.
//! Dossier source : target/release, ou VIHERD_DIST pour en choisir un autre.
use std::path::PathBuf;

#[path = "../src/icon.rs"]
#[allow(dead_code)]
mod icon;

fn main() {
    // icône de viherd-setup.exe
    println!("cargo:rerun-if-changed=../src/icon.rs");
    let ico = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("viherd.ico");
    std::fs::write(&ico, icon::ico()).expect("viherd.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico.to_str().unwrap())
        .set("ProductName", "Viherd")
        .set("FileDescription", "Installeur de Viherd");
    res.compile().expect("ressources Windows (icône) : rc.exe du SDK Windows introuvable ?");

    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("..");
    let dist = std::env::var("VIHERD_DIST").map(PathBuf::from).unwrap_or_else(|_| root.join("target").join("release"));
    println!("cargo:rerun-if-env-changed=VIHERD_DIST");
    for (var, file) in [("VIHERD_CLI_EXE", "viherd.exe"), ("VIHERD_APP_EXE", "viherd-app.exe")] {
        let p = dist.join(file);
        if !p.is_file() {
            panic!(
                "{} introuvable : compile d'abord Viherd en release (`cargo build --release --bin viherd --bin viherd-app`) \
                 ou lance .\\build-installer.ps1",
                p.display()
            );
        }
        println!("cargo:rerun-if-changed={}", p.display());
        let abs = p.canonicalize().unwrap().display().to_string();
        println!("cargo:rustc-env={var}={}", abs.trim_start_matches(r"\\?\"));
    }
}
