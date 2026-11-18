//! Z.ai Coding Plan(API Key 形态,国际区)适配器。
//!
//! 与 [`zcode`](super::zcode) 的分工:zcode 走 ZCode 客户端登录态 JWT
//! (`zcode.z.ai/api/v1/zcode-plan/...` 专用端点族);本类型走 **z.ai 开放
//! 平台发放的 Coding Plan API Key + 官方 Anthropic 兼容端点**
//! `https://api.z.ai/api/anthropic`(2026-11-18 官方配置文档;同一凭证
//! 也有 OpenAI 形态 `/api/coding/paas/v4`,网关统一挂 Anthropic 协议,
//! 跨协议转换由管线完成)。
//!
//! 端点形状 = 标准 Anthropic(messages = `{base}/v1/messages`,
//! `/models` 公开 200,健康探测可用),因此全部继承 generic 默认实现;
//! 本适配器只钉 kind/说明,供反代应用中心归组与预设填充。
//!
//! 区域注意(用户裁决:国内国外都支持):国际区凭证在 z.ai 签发,
//! **与国内 BigModel 互不通用**;国内版用 [`bigmodel_plan`](super::bigmodel_plan)。

use crate::ChannelAdapter;

pub struct ZaiPlanAdapter;

pub const KIND: &str = "zai-plan";

impl ChannelAdapter for ZaiPlanAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> &'static str {
        "Z.ai Coding Plan(API Key)"
    }

    fn description(&self) -> &'static str {
        "国际区 GLM Coding Plan 的 API Key 形态:Key 在 z.ai 平台签发,\
         Base URL 填 https://api.z.ai/api/anthropic,只勾选 Anthropic 协议;\
         与 ZCode 客户端订阅(zcode 类型)同一套餐额度池的官方兼容端点,\
         也与国内 BigModel 凭证互不通用"
    }
}
