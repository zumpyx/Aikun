//! ZCode MCP 远程工具:路径前缀、服务目录与客户端头转发白名单。
//!
//! 形态炼化自 ZCode 官方插件 `.mcp.json`(2026-11-18 实测):远程 MCP
//! 服务挂 `{base}/api/v1/mcp/server/{server_key}`,Streamable HTTP
//! (JSON-RPC 2.0 over POST),鉴权 = 渠道同一 JWT(`zcode_official /
//! jwt_token`),消耗套餐权限(`requiresPaidPlan`)。网关透传实现见
//! `aikun/src/mcp.rs`。

// ---- MCP 远程工具 --------------------------------------------------------
// ZCode 官方插件 `.mcp.json` 揭示的远程 MCP 服务形态(2026-11-18 本地
// 插件缓存实测):`{ZCODE_BASE_URL}/api/v1/mcp/server/{server_key}`,HTTP
// 传输(MCP Streamable HTTP,JSON-RPC),`auth: zcode_official /
// jwt_token` = 同一 ZCode 登录态 JWT,`requiresPaidPlan: true`(消耗套餐
// 权限)。网关按此把 MCP 请求透传给 zcode 渠道(见 aikun/src/mcp.rs)。

/// MCP 远程服务路径前缀;拼接规则 `join_absolute(base, prefix + server_key)`。
pub const MCP_SERVER_PATH_PREFIX: &str = "/api/v1/mcp/server/";

/// 已知 MCP 服务目录(本地官方插件枚举;目录仅用于文档展示,网关不封死
/// ——任意 server_key 照常透传,存在性由上游裁决)。
pub const MCP_SERVERS: &[(&str, &str)] = &[
    (
        "finance_tianyancha",
        "天眼查企业信息查询(工商/股权/涉诉/舆情)",
    ),
    ("image_search", "图片搜索"),
];

/// 官方闲时窗口(北京时间):每晚 23:00 至次日 09:00,含头不含尾。
/// ZCode 夜间畅用:窗口内 GLM-5.3-Flash 套餐额度消耗为 0。
pub const MCP_FORWARDED_HEADERS: &[&str] = &["mcp-session-id", "mcp-protocol-version", "accept"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_catalog_and_forwarded_headers() {
        assert!(MCP_SERVERS.iter().any(|(k, _)| *k == "finance_tianyancha"));
        assert!(MCP_FORWARDED_HEADERS.contains(&"mcp-session-id"));
        assert_eq!(
            format!("{MCP_SERVER_PATH_PREFIX}finance_tianyancha"),
            "/api/v1/mcp/server/finance_tianyancha"
        );
    }
}
