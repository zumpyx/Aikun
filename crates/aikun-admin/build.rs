//! 前端资产嵌入(React 重写,2026-11-18):扫描仓库根 frontend/dist/
//! 生成 `OUT_DIR/frontend_assets.rs`(`INDEX_HTML` + `STATIC_ASSETS` 表,
//! 全部 include_str!),auth.rs include! 消费——单二进制零外部文件纪律不变。
//!
//! dist 是**提交进仓库的构建产物**(与 crates/aikun-openapi 生成物同一
//! 纪律:cargo build 零外部工具);改前端 → `cd frontend && npm run build`
//! → 提交 dist。缺失 dist 视为仓库不完整,直接报错。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let dist = PathBuf::from("../../frontend/dist");
    println!("cargo:rerun-if-changed=../../frontend/dist");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let index = dist.join("index.html");
    if !index.is_file() {
        panic!(
            "frontend/dist/index.html 不存在——dist 是提交进仓库的构建产物,\
             若缺失请先 `cd frontend && npm install && npm run build` 并提交 dist/"
        );
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
