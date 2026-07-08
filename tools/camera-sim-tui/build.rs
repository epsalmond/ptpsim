use std::process::Command;

fn main() {
    let rustc = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|version| version.trim().to_string())
        .unwrap_or_else(|| "rustc unknown".to_string());
    println!("cargo:rustc-env=PTPSIM_TUI_RUSTC_VERSION={rustc}");
}
