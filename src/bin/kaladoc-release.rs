//! `kaladoc-release` — build, package, and publish KalaDoc binaries.
//!
//! Subcommands:
//!
//! ```text
//! kaladoc-release check                          # verify build deps
//! kaladoc-release targets                        # list release targets
//! kaladoc-release package --target SLUG --binary PATH [--version V] [--out DIR] [--name NAME]
//! kaladoc-release build   --target SLUG[,SLUG..] [--version V] [--out DIR] [--name NAME]
//! kaladoc-release aggregate --dir DIR [--version V] [--name NAME]
//! kaladoc-release publish --tag TAG --dir DIR
//! ```
//!
//! Env fallbacks: `KALADOC_RELEASE_VERSION`, `KALADOC_RELEASE_TARGETS`,
//! `KALADOC_RELEASE_OUT_DIR`, `KALADOC_RELEASE_TAG`,
//! `KALADOC_RELEASE_PACKAGE_NAME`.

use std::path::PathBuf;
use std::process::ExitCode;

use kaladoc::deployments::package::{PackageSpec, ReleaseTarget, package};
use kaladoc::deployments::release::{ReleaseConfig, aggregate, build_release, publish};
use kaladoc::deployments::require::{check_requirements, install_hint};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        print_usage();
        return ExitCode::from(2);
    };
    let rest = &args[1..];

    let result = match cmd.as_str() {
        "check" => cmd_check(),
        "targets" => cmd_targets(),
        "package" => cmd_package(rest),
        "build" => cmd_build(rest),
        "aggregate" => cmd_aggregate(rest),
        "publish" => cmd_publish(rest),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            print_usage();
            Ok(())
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: kaladoc-release <check|targets|package|build|aggregate|publish> [options]\n\
         run `kaladoc-release <cmd> --help`-style flags are documented in src/bin/kaladoc-release.rs"
    );
}

fn cmd_check() -> Result<(), String> {
    let report = check_requirements();
    println!("os: {:?}  distro: {:?}", report.os, report.distro);
    for dep in &report.dependencies {
        println!(
            "  {:<8} {} {}",
            dep.name,
            if dep.installed { "ok" } else { "MISSING" },
            dep.detected_version.as_deref().unwrap_or("")
        );
    }
    if !report.missing.is_empty() {
        println!("\ninstall hints:");
        for hint in install_hint(&report) {
            println!("  {hint}");
        }
        return Err(format!(
            "missing dependencies: {}",
            report.missing.join(", ")
        ));
    }
    Ok(())
}

fn cmd_targets() -> Result<(), String> {
    for t in ReleaseTarget::all() {
        println!("{:<16} {}", t.slug(), t.triple());
    }
    Ok(())
}

struct Flags {
    values: Vec<(String, String)>,
}

impl Flags {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut values = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(name) = a.strip_prefix("--") {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| format!("--{name} requires a value"))?;
                values.push((name.to_string(), value.clone()));
                i += 2;
            } else {
                return Err(format!("unexpected argument: {a}"));
            }
        }
        Ok(Self { values })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn require(&self, name: &str) -> Result<String, String> {
        self.get(name)
            .map(str::to_string)
            .ok_or_else(|| format!("missing required --{name}"))
    }
}

fn base_config(flags: &Flags) -> Result<ReleaseConfig, String> {
    let mut cfg = ReleaseConfig::from_env();
    if let Some(v) = flags.get("version") {
        cfg.version = v.to_string();
    }
    if let Some(v) = flags.get("out") {
        cfg.out_dir = PathBuf::from(v);
    }
    if let Some(v) = flags.get("name") {
        cfg.package_name = v.to_string();
    }
    Ok(cfg)
}

fn parse_targets(flags: &Flags) -> Result<Vec<ReleaseTarget>, String> {
    let raw = flags
        .get("target")
        .ok_or("missing required --target")?
        .to_string();
    let parsed: Vec<ReleaseTarget> = raw
        .split(',')
        .map(str::trim)
        .map(|s| {
            ReleaseTarget::from_slug(s)
                .ok_or_else(|| format!("unknown target slug: {s} (see `kaladoc-release targets`)"))
        })
        .collect::<Result<_, _>>()?;
    if parsed.is_empty() {
        return Err("--target produced no targets".into());
    }
    Ok(parsed)
}

fn cmd_package(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args)?;
    let target_slug = flags.require("target")?;
    let target = ReleaseTarget::from_slug(&target_slug)
        .ok_or_else(|| format!("unknown target slug: {target_slug}"))?;
    let binary = PathBuf::from(flags.require("binary")?);
    let cfg = base_config(&flags)?;

    let spec = PackageSpec::new(&binary, &cfg.version, target)
        .with_out_dir(&cfg.out_dir)
        .with_package_name(&cfg.package_name);
    let arts = package(&spec).map_err(|e| e.to_string())?;
    println!("packaged {}", arts.package_dir_name);
    println!(
        "  {}  ({} bytes, sha256 {})",
        arts.tar_gz.path.display(),
        arts.tar_gz.size_bytes,
        &arts.tar_gz.sha256[..16]
    );
    println!(
        "  {}  ({} bytes, sha256 {})",
        arts.zip.path.display(),
        arts.zip.size_bytes,
        &arts.zip.sha256[..16]
    );
    Ok(())
}

fn cmd_build(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args)?;
    let mut cfg = base_config(&flags)?;
    cfg.targets = parse_targets(&flags)?;
    if let Some(tag) = flags.get("tag") {
        cfg.tag = Some(tag.to_string());
    }

    let arts = build_release(&cfg).map_err(|e| e.to_string())?;
    for a in &arts {
        println!(
            "{}: {} + {}",
            a.target.slug(),
            a.tar_gz.path.display(),
            a.zip.path.display()
        );
    }
    Ok(())
}

fn cmd_aggregate(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args)?;
    let dir = PathBuf::from(flags.require("dir")?);
    let cfg = base_config(&flags)?;
    let entries =
        aggregate(&dir, &cfg.package_name, Some(&cfg.version)).map_err(|e| e.to_string())?;
    println!(
        "aggregated {} artifact(s) in {}",
        entries.len(),
        dir.display()
    );
    println!("  wrote SHA256SUMS and manifest.json");
    Ok(())
}

fn cmd_publish(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args)?;
    let tag = flags.require("tag")?;
    let dir = PathBuf::from(flags.require("dir")?);
    publish(&dir, &tag).map_err(|e| e.to_string())?;
    println!("published {} to GitHub release {}", dir.display(), tag);
    Ok(())
}
