#!/usr/bin/env python3
"""Type-check the actual resource module without platform C/linker dependencies.
This deliberately does not claim that the complete crate or native runtime passed.
"""
from pathlib import Path
import json
import os
import subprocess
import sys
import tempfile
module = Path(__file__).resolve().parents[1] / "src/resources.rs"
with tempfile.TemporaryDirectory(prefix="excalibur-resource-adapters-") as temp:
    root = Path(temp)
    (root / "src").mkdir()
    (root / "Cargo.toml").write_text('''[package]
name="excalibur-resource-portability-check"
version="0.0.0"
edition="2024"
[dependencies]
serde={version="1",features=["derive"]}
tokio={version="1",features=["rt","sync"]}
libc="0.2"
[target.'cfg(windows)'.dependencies]
windows-sys={version="0.61",features=["Win32_Foundation","Win32_System_ProcessStatus","Win32_System_Threading","Win32_System_JobObjects"]}
''')
    (root / "src/lib.rs").write_text('''#[derive(Debug)]
pub enum Error {Invalid(&'static str)}
pub type Result<T> = std::result::Result<T,Error>;
#[path=''' + json.dumps(str(module)) + ''']
pub mod resources;
''')
    for target in sys.argv[1:] or ["x86_64-pc-windows-gnu", "aarch64-apple-darwin"]:
        subprocess.run(["cargo", "check", "--manifest-path", str(root / "Cargo.toml"), "--target", target], check=True,
                       env={**os.environ, "CARGO_TARGET_DIR":str(root / "target")})
