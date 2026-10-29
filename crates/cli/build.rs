use std::process::Command;
fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads/main");
    if let Ok(out) = Command::new("git").args(["rev-parse", "HEAD"]).output()
        && out.status.success()
    {
        println!(
            "cargo:rustc-env=EXCALIBUR_BUILD_COMMIT={}",
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    if let Ok(out) = Command::new("git").args(["status", "--porcelain"]).output() {
        println!(
            "cargo:rustc-env=EXCALIBUR_BUILD_DIRTY={}",
            !out.stdout.is_empty()
        );
    }
}
