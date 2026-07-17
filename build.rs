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

/// Compile csrc/rail_bridge.c and link Microsoft's FreeRDP fork (FreeRDP 2.x,
/// with the RAIL/VAIL server-side extensions that WSLg's Weston speaks), built
/// and installed separately — see docs. Upstream FreeRDP 3 (Homebrew) does *not*
/// interoperate with WSLg's RAIL stream, so the fork is required here.
///
/// The fork's install prefix defaults to ~/.local/msfreerdp and is overridable
/// via the MSFREERDP_PREFIX env var.
fn build_rail_bridge() {
    println!("cargo:rerun-if-changed=csrc/rail_bridge.c");
    println!("cargo:rerun-if-changed=csrc/rail_bridge.h");
    println!("cargo:rerun-if-env-changed=MSFREERDP_PREFIX");

    let prefix = std::env::var("MSFREERDP_PREFIX").unwrap_or_else(|_| {
        let home = std::env::var("HOME").expect("HOME");
        format!("{home}/.local/msfreerdp")
    });
    let mut pkg_config_path = format!("{prefix}/lib/pkgconfig");
    if let Ok(existing) = std::env::var("PKG_CONFIG_PATH") {
        if !existing.is_empty() {
            pkg_config_path.push(':');
            pkg_config_path.push_str(&existing);
        }
    }
    // The fork's dylibs use @rpath install names; add its lib dir as an rpath so
    // the final binary resolves them at runtime.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{prefix}/lib");

    let pkgs = ["freerdp2", "freerdp-client2", "winpr2"];
    let pc = |flag: &str| -> Vec<String> {
        let out = Command::new("pkg-config")
            .env("PKG_CONFIG_PATH", &pkg_config_path)
            .arg(flag)
            .args(pkgs)
            .output()
            .unwrap_or_else(|e| {
                panic!(
                    "failed to run pkg-config for the FreeRDP fork at {prefix} \
                     (build microsoft/FreeRDP-mirror and install it there, or set \
                     MSFREERDP_PREFIX): {e}"
                )
            });
        if !out.status.success() {
            panic!(
                "pkg-config {flag} {} failed — the Microsoft FreeRDP fork isn't at \
                 {prefix}. Build microsoft/FreeRDP-mirror (2.x, RAIL/VAIL) and install \
                 it there, or set MSFREERDP_PREFIX.\n{}",
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
