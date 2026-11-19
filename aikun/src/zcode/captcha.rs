//! ZCode 领取的阿里验证码求解(claim 3007 必带;chat 不需要——
//! 上游 skip_model_request)。
//!
//! 实现炼化自 Zcode2Api internal/captcha(nodesolver.go + captcha_node/):
//! **Node 子进程跑 happy-dom 模拟浏览器**,在真实 zcode.z.ai origin 上
//! 把阿里 FeiLin/pe.* 设备指纹 VM 跑完,mint 出
//! `X-Aliyun-Captcha-Verify-Param`(2026-11-19 本机实测可用,手动 claim
//! 200 通过)。pe-stall(exit 2)是脚本生命周期常态,重试即过。
//!
//! 文件布局:solver.js + package.json 经 `include_str!` 嵌入二进制
//! (单二进制纪律),首次使用释放到 `<data>/captcha_node/` 并
//! `npm install`(需要 node ≥ 20;不可用时领取报可读错误,不影响其它
//! 功能)。CDN 缓存由 solver 自管(~/.zcode-captcha-cdn-cache)。
//!
//! scene/region/prefix:GET `/api/v1/client/configs`(公开,5 分钟缓存)
//! 的 captcha 段,解析失败回退实测默认(11xygtvd / cn / no8xfe)。

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// 嵌入的 solver 脚本(MIT 级公开仓库 Zcode2Api captcha_node,炼化引用)。
const SOLVER_JS: &str = include_str!("captcha_node/solver.js");
const SOLVER_PKG: &str = include_str!("captcha_node/package.json");

/// client/configs 的 captcha 段(实测默认:region cn——旧值 sgp 已过期)。
#[derive(Debug, Clone)]
pub struct CaptchaCfg {
    pub enabled: bool,
    pub prefix: String,
    pub region: String,
    pub scene_id: String,
}

impl Default for CaptchaCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            prefix: "no8xfe".into(),
            region: "cn".into(),
            scene_id: "11xygtvd".into(),
        }
    }
}

static CFG_CACHE: OnceLock<Mutex<Option<(CaptchaCfg, Instant)>>> = OnceLock::new();

fn cfg_cache() -> &'static Mutex<Option<(CaptchaCfg, Instant)>> {
    CFG_CACHE.get_or_init(|| Mutex::new(None))
}

/// solver 安装目录(`<data>/captcha_node`;data 目录口径同主程序)。
fn solver_dir() -> PathBuf {
    let base = std::env::var("AIKUN_DATA_DIR").unwrap_or_else(|_| "data".to_string());
    PathBuf::from(base).join("captcha_node")
}

/// 释放嵌入文件 + 首次 npm install;返回 solver 目录(失败带可读错误)。
/// 同步阻塞(spawn_blocking 里调用)。
fn ensure_solver() -> Result<PathBuf, String> {
    let dir = solver_dir();
    let js = dir.join("solver.js");
    let pkg = dir.join("package.json");
    if !js.is_file() || !pkg.is_file() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {dir:?} 失败: {e}"))?;
        std::fs::write(&js, SOLVER_JS).map_err(|e| format!("释放 solver.js 失败: {e}"))?;
        std::fs::write(&pkg, SOLVER_PKG).map_err(|e| format!("释放 package.json 失败: {e}"))?;
    }
    if !dir.join("node_modules").is_dir() {
        let ok = std::process::Command::new("npm")
            .args(["install", "--no-audit", "--no-fund", "--loglevel=error"])
            .current_dir(&dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return Err(format!(
                "captcha solver 首次安装失败(需要 node ≥ 20 与 npm;目录 {dir:?})"
            ));
        }
    }
    Ok(dir)
}

/// 拉取并缓存 captcha 配置(client/configs;失败用默认,不阻塞领取)。
pub async fn captcha_cfg(base: &str) -> CaptchaCfg {
    if let Some((cfg, at)) = cfg_cache().lock().unwrap().clone()
        && at.elapsed() < Duration::from_secs(5 * 60)
    {
        return cfg;
    }
    let url = format!("{base}/api/v1/client/configs?app_version=3.18.2&platform=darwin-x64");
    let cfg = match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => match c.get(&url).send().await {
            Ok(r) => r.json::<Value>().await.unwrap_or(Value::Null),
            Err(_) => Value::Null,
        },
        Err(_) => Value::Null,
    };
    let mut cfg_out = CaptchaCfg::default();
    let cc = &cfg["data"]["configs"]["captcha"];
    if cc["prefix"].as_str().is_some_and(|p| !p.is_empty()) {
        cfg_out = CaptchaCfg {
            enabled: cc["enabled"].as_bool().unwrap_or(true),
            prefix: cc["prefix"].as_str().unwrap_or_default().to_string(),
            region: cc["region"].as_str().unwrap_or("cn").to_string(),
            scene_id: cc["sceneId"].as_str().unwrap_or_default().to_string(),
        };
    }
    *cfg_cache().lock().unwrap() = Some((cfg_out.clone(), Instant::now()));
    cfg_out
}

/// mint 一次验证码参数:子进程跑 solver,stdout 里取 VERIFY_PARAM。
/// pe-stall(exit 2)常态,重试最多 `attempts` 次(实测第 1-2 次常见 stall)。
/// 子进程最长 90s/次,整体经 spawn_blocking 跑,不占 async executor。
pub async fn solve(base: &str, attempts: u32) -> Result<(String, String), String> {
    let cfg = captcha_cfg(base).await;
    if !cfg.enabled {
        return Err("captcha disabled by origin config".into());
    }
    tokio::task::spawn_blocking(move || {
        let dir = ensure_solver()?;
        for attempt in 1..=attempts.max(1) {
            let out = std::process::Command::new("node")
                .arg("solver.js")
                .arg(&cfg.scene_id)
                .arg(&cfg.region)
                .arg(&cfg.prefix)
                .current_dir(&dir)
                .env_remove("http_proxy")
                .env_remove("https_proxy")
                .env_remove("all_proxy")
                .env_remove("HTTP_PROXY")
                .env_remove("HTTPS_PROXY")
                .env_remove("ALL_PROXY")
                .output()
                .map_err(|e| format!("启动 node solver 失败(需要 node ≥ 20): {e}"))?;
            if let Some(p) = String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|l| l.strip_prefix("VERIFY_PARAM=").filter(|p| !p.is_empty()))
            {
                return Ok((p.to_string(), cfg.region.clone()));
            }
            tracing::debug!(
                "captcha solver attempt {attempt}/{} 未出参(exit={}): {}",
                attempts.max(1),
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("")
            );
        }
        Err(format!(
            "captcha solver {attempts} 次未产出 VERIFY_PARAM(pe-stall 连发;稍后重试或检查 node/网络)"
        ))
    })
    .await
    .map_err(|e| format!("captcha 求解任务失败: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cfg_shape() {
        let c = CaptchaCfg::default();
        assert_eq!(c.region, "cn");
        assert_eq!(c.scene_id, "11xygtvd");
        assert!(c.enabled);
    }
}
