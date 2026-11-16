//! 上游 URL 拼接(Azure api-version、已有版本段、完整端点 URL 三种形态)。
//! 从 transport 移入:纯字符串逻辑,属渠道域;transport 经 re-export 保持旧路径。

pub fn build_api_url(base_url: &str, path: &str) -> String {
    // base_url 可能带 query（如 Azure 的 ?api-version=...），先拆出来最后拼回
    let (base, query) = match base_url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (base_url, None),
    };
    let base = base.trim_end_matches('/');
    let joined = if is_endpoint_url(base) {
        // base 已含完整端点路径（如 Azure 部署 URL），原样使用
        base.to_string()
    } else if is_version_segment(base.rsplit('/').next().unwrap_or("")) {
        // 已含版本段（/v1、/v1beta、/v2 等），不再追加 /v1
        format!("{}{}", base, path)
    } else {
        format!("{}/v1{}", base, path)
    };
    match query {
        Some(q) => format!("{}?{}", joined, q),
        None => joined,
    }
}

fn is_endpoint_url(base: &str) -> bool {
    ["/chat/completions", "/messages", "/models", "/responses"]
        .iter()
        .any(|ep| base.ends_with(ep))
}

fn is_version_segment(seg: &str) -> bool {
    seg.len() >= 2
        && seg.starts_with('v')
        && seg.as_bytes()[1].is_ascii_digit()
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}
