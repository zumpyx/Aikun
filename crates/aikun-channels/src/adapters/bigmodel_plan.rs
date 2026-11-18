//! BigModel Coding Plan(API Key 形态,国内区)适配器。
//!
//! 国内区 GLM Coding Plan 在智谱开放平台(bigmodel.cn)签发 API Key,
//! 官方 Anthropic 兼容端点 `https://open.bigmodel.cn/api/anthropic`
//! (2026-11-18 官方配置文档;`/v1/messages` 与公开 `/models` 已实测,
//! 后者未鉴权 200,健康探测可用)。端点形状 = 标准 Anthropic,全部继承
//! generic 默认实现;本适配器只钉 kind/说明。
//!
//! 区域注意:国内凭证与 z.ai(国际区)互不通用;国内 ZCode 客户端
//! 订阅用户请用 [`zcode`](super::zcode) 类型 + 国内 Base
//! `https://zcode.bigmodel.cn`(域名实测存在,路径族与国际版同构)。

use crate::ChannelAdapter;

pub struct BigModelPlanAdapter;

pub const KIND: &str = "bigmodel-plan";

impl ChannelAdapter for BigModelPlanAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> &'static str {
        "BigModel Coding Plan(API Key)"
    }

    fn description(&self) -> &'static str {
        "国内区 GLM Coding Plan 的 API Key 形态:Key 在智谱开放平台\
         (bigmodel.cn)签发,Base URL 填 https://open.bigmodel.cn/api/anthropic,\
         只勾选 Anthropic 协议;与国际区 z.ai 凭证互不通用"
    }
}
