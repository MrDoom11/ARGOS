fn main() {
    println!("cargo:rerun-if-changed=argos.rc");
    println!("cargo:rerun-if-changed=assets/argos.ico");

    #[cfg(windows)]
    embed_icon();
}

#[cfg(windows)]
fn embed_icon() {
    use std::{env, path::PathBuf};

    let target = env::var("TARGET").expect("Cargo deve informar a plataforma de destino");
    if !target.ends_with("-windows-msvc") {
        return;
    }

    let resource = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo deve informar OUT_DIR"))
        .join("argos.res");
    let status = find_msvc_tools::find(&target, "rc.exe")
        .expect("O compilador de recursos rc.exe é fornecido pelo SDK do Windows")
        .args(["/nologo", "/fo"])
        .arg(&resource)
        .arg("argos.rc")
        .status()
        .expect("Não foi possível executar rc.exe");
    assert!(
        status.success(),
        "Falha ao compilar o ícone do ARGOS ({status})"
    );

    for binary in ["argos", "argos-gui"] {
        println!("cargo:rustc-link-arg-bin={binary}={}", resource.display());
    }
}
