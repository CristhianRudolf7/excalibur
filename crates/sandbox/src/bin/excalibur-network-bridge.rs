//! Internal sandbox bootstrap adapter; not an Excalibur product interface.
#[cfg(target_os = "linux")]
fn main() {
    let code = match excalibur_sandbox::bridge::run_arguments(std::env::args_os().skip(1)) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            125
        }
    };
    std::process::exit(code);
}
#[cfg(not(target_os = "linux"))]
fn main() {
    std::process::exit(125);
}
