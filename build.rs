use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo:rerun-if-changed=assets/ssdownload.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/app.manifest");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let version = env::var("CARGO_PKG_VERSION").unwrap();
        let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
        let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let manifest = out.join("app.manifest");
        fs::write(
            &manifest,
            fs::read_to_string(root.join("assets/app.manifest"))
                .unwrap()
                .replace("@VERSION_QUAD@", &format!("{version}.0")),
        )
        .unwrap();
        let rc = fs::read_to_string(root.join("assets/ssdownload.rc"))
            .unwrap()
            .replace(
                "@VERSION_COMMAS@",
                &format!("{},0", version.replace('.', ",")),
            )
            .replace("@VERSION@", &version)
            .replace(
                r"assets\\app.ico",
                &root
                    .join("assets/app.ico")
                    .to_string_lossy()
                    .replace('\\', "/"),
            )
            .replace(
                r"assets\\app.manifest",
                &manifest.to_string_lossy().replace('\\', "/"),
            );
        let generated = out.join("ssdownload.rc");
        fs::write(&generated, rc).unwrap();
        embed_resource::compile_for(
            &generated,
            ["ssdownload", "ssdownload-cli"],
            embed_resource::NONE,
        )
        .manifest_required()
        .expect("SSDownload Windows resources could not be compiled");
    }
}
