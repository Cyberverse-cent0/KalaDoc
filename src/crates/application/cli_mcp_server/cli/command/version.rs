//! `kaladoc version`

use serde::Serialize;

use super::{CliError, Context};

#[derive(Debug, Serialize)]
struct VersionInfo {
    name: &'static str,
    version: &'static str,
    build: &'static str,
}

pub fn run(ctx: &Context) -> Result<i32, CliError> {
    let info = VersionInfo {
        name: "kaladoc",
        version: env!("CARGO_PKG_VERSION"),
        build: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    };

    if ctx.emit_structured(&info)? {
        return Ok(0);
    }
    println!("{} {}", info.name, info.version);
    println!("build: {}", info.build);
    Ok(0)
}
