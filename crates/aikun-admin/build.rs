//! 前端资产嵌入:编译时先跑 `npm run build`，再把 frontend/dist 嵌进二进制。
//! dist 不入库。改前端后直接 `cargo build` / `cargo run` 即可。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let frontend = PathBuf::from("../../frontend");
    println!("cargo:rerun-if-changed=../../frontend/src");
    println!("cargo:rerun-if-changed=../../frontend/index.html");
    println!("cargo:rerun-if-changed=../../frontend/package.json");
    println!("cargo:rerun-if-changed=../../frontend/package-lock.json");
    println!("cargo:rerun-if-changed=../../frontend/vite.config.ts");
    println!("cargo:rerun-if-changed=../../frontend/tsconfig.json");

    build_frontend(&frontend);

    let dist = frontend.join("dist");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let index = dist.join("index.html");
    if !index.is_file() {
        panic!("frontend/dist/index.html 不存在——npm run build 未产出");
    }

    let mut assets = vec![];
    collect_assets(&dist.join("assets"), &dist, &mut assets);
    assets.sort();

    let mut rs = String::new();
    let _ = writeln!(rs, "// 由 build.rs 从 frontend/dist 生成,勿手改。");
    let _ = writeln!(
        rs,
        "pub const INDEX_HTML: &str = include_str!(r#\"{}\"#);",
        index.canonicalize().unwrap().display()
    );
    let _ = writeln!(rs, "pub static STATIC_ASSETS: &[(&str, &str)] = &[");
    for (rel, abs) in &assets {
        let _ = writeln!(
            rs,
            "    (\"{rel}\", include_str!(r#\"{}\"#)),",
            abs.canonicalize().unwrap().display()
        );
    }
    let _ = writeln!(rs, "];");

    std::fs::write(out_dir.join("frontend_assets.rs"), rs).expect("write frontend_assets.rs");
}

fn build_frontend(frontend: &Path) {
    if !frontend.join("node_modules").is_dir() {
        let status = Command::new("npm")
            .args(["ci"])
            .current_dir(frontend)
            .status()
            .unwrap_or_else(|e| panic!("找不到 npm({e})。请安装 Node.js 22+"));
        if !status.success() {
            panic!("npm ci 失败");
        }
    }
    let status = Command::new("npm")
        .args(["run", "build"])
        .current_dir(frontend)
        .status()
        .unwrap_or_else(|e| panic!("找不到 npm({e})。请安装 Node.js 22+"));
    if !status.success() {
        panic!("npm run build 失败");
    }
}

/// 递归收集 css/js(mime 表只覆盖这两类,与 asset_mime 口径一致)。
fn collect_assets(dir: &Path, dist: &Path, out: &mut Vec<(String, PathBuf)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_assets(&path, dist, out);
        } else if let Some("css" | "js") = path.extension().and_then(|e| e.to_str()) {
            let rel = path
                .strip_prefix(dist)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, path));
        }
    }
}
