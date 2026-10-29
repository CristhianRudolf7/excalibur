use excalibur_webd::{Config, drop_privileges, preflight};
fn run() -> Result<bool, String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 || args[0] != "--preflight" {
        return Err("usage: excalibur-webd --preflight <config.toml>".into());
    }
    let config = Config::parse(&std::fs::read_to_string(&args[1]).map_err(|e| e.to_string())?)?;
    let identity = drop_privileges(&config.web.os_user)?;
    let report = preflight(&config.web, identity);
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
    );
    Ok(report.ready)
}
fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            println!("{}", serde_json::json!({"ready": false, "error": error}));
            std::process::exit(1);
        }
    }
}
