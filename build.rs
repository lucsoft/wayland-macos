// libxkbcommon is provided by Homebrew, which isn't on the default linker search
// path on macOS. Point the linker at it (arch-independent via `brew --prefix`,
// with the usual Apple-silicon/Intel fallbacks).
use std::process::Command;

fn main() {
    if let Ok(out) = Command::new("brew").arg("--prefix").output() {
        if out.status.success() {
            let prefix = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !prefix.is_empty() {
                println!("cargo:rustc-link-search=native={prefix}/lib");
            }
        }
    }
    println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
    println!("cargo:rustc-link-search=native=/usr/local/lib");

    // The RAIL back-end links FreeRDP; only build its C bridge when the feature
    // is enabled (see Cargo.toml [features].rail).
    if std::env::var("CARGO_FEATURE_RAIL").is_ok() {
        build_rail_bridge();
    }
}

/// Compile csrc/rail_bridge.c and link FreeRDP 3 / WinPR, discovered via
/// pkg-config (with FreeRDP's Homebrew keg added to PKG_CONFIG_PATH).
fn build_rail_bridge() {
    println!("cargo:rerun-if-changed=csrc/rail_bridge.c");
    println!("cargo:rerun-if-changed=csrc/rail_bridge.h");

    let freerdp_prefix = brew_prefix("freerdp");
    let mut pkg_config_path = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();
    if let Some(p) = &freerdp_prefix {
        if !pkg_config_path.is_empty() {
            pkg_config_path.push(':');
        }
        pkg_config_path.push_str(&format!("{p}/lib/pkgconfig"));
    }

    let pkgs = ["freerdp3", "freerdp-client3", "winpr3"];
    let pc = |flag: &str| -> Vec<String> {
        let out = Command::new("pkg-config")
            .env("PKG_CONFIG_PATH", &pkg_config_path)
            .arg(flag)
            .args(pkgs)
            .output()
            .unwrap_or_else(|e| {
                panic!("failed to run pkg-config (is FreeRDP installed? `brew install freerdp`): {e}")
            });
        if !out.status.success() {
            panic!(
                "pkg-config {flag} {} failed — is FreeRDP installed? `brew install freerdp`.\n{}",
                pkgs.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect()
    };

    let mut build = cc::Build::new();
    build.file("csrc/rail_bridge.c");
    for flag in pc("--cflags") {
        if let Some(inc) = flag.strip_prefix("-I") {
            build.include(inc);
        } else if let Some(def) = flag.strip_prefix("-D") {
            build.define(def, None);
        }
    }
    build.compile("rail_bridge");

    for flag in pc("--libs") {
        if let Some(dir) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={dir}");
        } else if let Some(name) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={name}");
        }
    }
}

fn brew_prefix(pkg: &str) -> Option<String> {
    let out = Command::new("brew").args(["--prefix", pkg]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        None
    } else {
        Some(p)
    }
}
