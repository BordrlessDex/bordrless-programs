//! LiteSVM harness for the Bordrless programs. The suites in `tests/` load the `.so` files that
//! `scripts/solana/programs.sh build` wrote into `<checkout>/target/deploy` (or `SBF_OUT_DIR`), so
//! they exercise the bytes that would be deployed.

pub mod env;
pub mod events;
pub mod fixture;
pub mod hooks;
pub mod kit;
pub mod launch;
pub mod spl;

pub use env::*;
pub use events::*;

use std::path::PathBuf;

/// The programs, by crate library name (the stem of each `.so`). `hook_tester` is test-only.
pub const PROGRAMS: [&str; 7] = [
    "bordrless_token",
    "bordrless_swap",
    "bordrless_bridge",
    "bordrless_launch",
    "bordrless_kit",
    "tax_hook",
    "hook_tester",
];

/// Root of the checkout.
pub fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into()),
    );
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("..").join(".."))
}

/// Directory holding the `.so` files.
pub fn sbf_out_dir() -> PathBuf {
    for name in ["SBF_OUT_DIR", "SBF_OUT_PATH"] {
        if let Some(dir) = std::env::var_os(name).filter(|d| !d.is_empty()) {
            return PathBuf::from(dir);
        }
    }
    workspace_root().join("target").join("deploy")
}

/// The bytes of a built program.
pub fn program_bytes(name: &str) -> Vec<u8> {
    let path = sbf_out_dir().join(format!("{name}.so"));
    std::fs::read(&path).unwrap_or_else(|err| {
        panic!(
            "cannot read {} ({err}); build first: scripts/solana/programs.sh build",
            path.display()
        )
    })
}
