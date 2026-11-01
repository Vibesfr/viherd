//! Icône et informations de version intégrées à viherd.exe et viherd-app.exe
//! (Explorateur, raccourcis, barre des tâches, « Applications installées »).
#[path = "src/icon.rs"]
#[allow(dead_code)]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ico = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("viherd.ico");
    std::fs::write(&ico, icon::ico()).expect("viherd.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico.to_str().unwrap())
        .set("ProductName", "Viherd")
        .set("FileDescription", "Viherd — environnement de dev PHP & Node")
        .set("CompanyName", "Viherd");
    res.compile().expect("ressources Windows (icône) : rc.exe du SDK Windows introuvable ?");
}
