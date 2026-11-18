//! ZCode 每日自动领取(签到)调度器。
//!
//! 官方免费/体验额度每日发放但不自动到账(见 zcode 渠道文档):本调度器
//! 每天在北京时间 `zcode_claim_hour` 点 10 分(错开整点发放竞态)对全部
//! 活跃 zcode 渠道串行跑一遍领取序列(渠道间 2s 间隔,降低上游风控/限流
//! 压力——Zcode2Api 对 mass claim 的警告),结果写 proxy_task 快照
//! (source=auto),签到面板与管理日志可见。`AIKUN_ZCODE_AUTO_CLAIM=false`
//! 可整体关闭。
//!
//! 只跑 `channel_kind = zcode`(JWT 订阅形态):zai-plan / bigmodel-plan
//! 是 API Key 形态,无领取序列可言。

use std::sync::Arc;

use crate::store::AppStore;
use aikun_openapi::openapi::server::errors::AdminZcodeClaimQuotaResponse;

/// 调度器启动入口(main.rs 装配后调用;内部 spawn,不阻塞)。
pub fn spawn_auto_claim(
    store: Arc<AppStore>,
    admin: aikun_admin::AdminService,
    config: &crate::config::AppConfig,
) {
    if !config.zcode_auto_claim {
        tracing::info!("zcode 每日自动领取已关闭(AIKUN_ZCODE_AUTO_CLAIM=false)");
        return;
    }
    let hour = config.zcode_claim_hour;
    tokio::spawn(async move {
        loop {
            let now = chrono::Utc::now().timestamp();
            let wait = secs_until_next_run(hour, 10, now);
            tracing::info!(
                "zcode 自动领取:下次执行于北京时间 {hour}:10(约 {} 小时后)",
                wait / 3600
            );
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            run_once(&store, &admin).await;
        }
    });
}

/// 一轮领取:活跃 zcode 渠道逐个来,渠道间 2s。
async fn run_once(store: &Arc<AppStore>, admin: &aikun_admin::AdminService) {
    let providers = match store.channels.active_channels().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("zcode 自动领取:读渠道失败: {e}");
            return;
        }
    };
    let targets: Vec<String> = providers
        .into_iter()
        .filter(|p| p.channel_kind.trim() == aikun_channels::adapters::zcode::KIND)
        .map(|p| p.id)
        .collect();
    if targets.is_empty() {
        tracing::info!("zcode 自动领取:无活跃 zcode 渠道,跳过");
        return;
    }
    tracing::info!("zcode 自动领取开始:{} 个渠道", targets.len());
    for (i, id) in targets.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        match admin
            .providers
            .admin_zcode_claim_quota_sourced(id.clone(), "auto")
            .await
        {
            AdminZcodeClaimQuotaResponse::Ok(r) if r.problems.is_empty() => {
                tracing::info!(
                    "zcode 自动领取({id}):claimed={:?} skipped={:?}",
                    r.claimed,
                    r.skipped
                );
            }
            AdminZcodeClaimQuotaResponse::Ok(r) => {
                tracing::warn!(
                    "zcode 自动领取({id})有问题:claimed={:?} skipped={:?} problems={:?}",
                    r.claimed,
                    r.skipped,
                    r.problems
                );
            }
            _other => {
                tracing::warn!("zcode 自动领取({id})未执行(渠道可能已删除或非 zcode)");
            }
        }
    }
}

/// 距下一次北京时间 `hour:minute` 的秒数(含头不含尾语义:恰好在目标
/// 时刻时顺延到明天)。北京 = UTC+8 无夏令时。
fn secs_until_next_run(hour: u32, minute: u32, now_unix: i64) -> u64 {
    let target_utc_secs_of_day = i64::from(hour) * 3600 + i64::from(minute) * 60 - 8 * 3600;
    let now_secs_of_day = now_unix.rem_euclid(86_400);
    let mut delta = target_utc_secs_of_day - now_secs_of_day;
    if delta <= 0 {
        delta += 86_400;
    }
    delta as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    // 北京 09:10 = UTC 01:10;构造已知锚点验证
    #[test]
    fn next_run_math() {
        // 北京 08:00(unix 对齐锚)= UTC 00:00 → 距 09:10 应为 1h10m
        let utc_midnight: i64 = 20_000 * 86_400;
        assert_eq!(secs_until_next_run(9, 10, utc_midnight), 3600 + 600);
        // 恰在北京 09:10(delta=0)→ 顺延一天
        let at_target = utc_midnight + 3600 + 600;
        assert_eq!(secs_until_next_run(9, 10, at_target), 86_400);
        // 北京 23:00(UTC 15:00)→ 跨零点到次日 09:10 = 10h10m
        let late = utc_midnight + 15 * 3600;
        assert_eq!(secs_until_next_run(9, 10, late), 10 * 3600 + 600);
    }
}
