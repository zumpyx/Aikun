//! Offline request-schema validators and normalized public rejections.
#![allow(dead_code)]
use super::errors::{InvalidParameter, ProblemDetails, RequestValidationRejection};
const VALIDATION_SCHEMA: &str = "{\"$defs\":{\"components\":{\"component_AdjustBalanceRequest\":{\"properties\":{\"amount\":{\"description\":\"正=充值,负=扣减(元);必须非零有限且 ≥1 微元\",\"format\":\"double\",\"type\":\"number\"},\"note\":{\"type\":\"string\"}},\"required\":[\"amount\"],\"type\":\"object\"},\"component_BatchCreateUserItem\":{\"properties\":{\"display_name\":{\"type\":\"string\"},\"password\":{\"description\":\"可省略;随机生成且仅在 per-item 结果返回一次\",\"type\":\"string\"},\"role\":{\"enum\":[\"admin\",\"user\"],\"type\":\"string\"},\"username\":{\"type\":\"string\"}},\"required\":[\"username\"],\"type\":\"object\"},\"component_BatchCreateUsersRequest\":{\"properties\":{\"users\":{\"items\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_BatchCreateUserItem\"},\"maxItems\":100,\"type\":\"array\"}},\"required\":[\"users\"],\"type\":\"object\"},\"component_CreateApiKeyRequest\":{\"properties\":{\"expires_at\":{\"description\":\"过期时间;**空串/缺省 = 永不过期**。非空值必须可解析且在未来,\\n否则 400 invalid_expires_at。解析口径宽松(parse_expiry):\\nRFC3339,或 YYYY-MM-DD[ HH:MM[:SS]]、YYYY-MM-DDTHH:MM。\\n\",\"type\":\"string\"},\"max_concurrent\":{\"format\":\"int64\",\"minimum\":0,\"type\":\"integer\"},\"models\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"},\"name\":{\"type\":\"string\"},\"quota_daily_tokens\":{\"format\":\"int64\",\"minimum\":0,\"type\":\"integer\"},\"rate_limit_rpm\":{\"format\":\"int64\",\"minimum\":0,\"type\":\"integer\"}},\"type\":\"object\"},\"component_CreateProviderRequest\":{\"description\":\"跨字段约束见 POST /api/providers 的 description(协议/base_url 联动、\\nprovider_type 白名单、channel_kind 注册表校验)。\\n\",\"properties\":{\"anthropic_base_url\":{\"type\":\"string\"},\"api_key\":{\"description\":\"入库静态加密(enc:v1:);key_hash 用于重复检测\",\"type\":\"string\"},\"channel_kind\":{\"type\":\"string\"},\"default_protocol\":{\"enum\":[\"openai\",\"anthropic\"],\"type\":\"string\"},\"max_retries\":{\"maximum\":10,\"minimum\":0,\"type\":\"integer\"},\"model_mapping\":{\"additionalProperties\":{\"type\":\"string\"},\"type\":\"object\"},\"models\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"},\"name\":{\"type\":\"string\"},\"note\":{\"type\":\"string\"},\"openai_base_url\":{\"type\":\"string\"},\"priority\":{\"maximum\":100,\"minimum\":0,\"type\":\"integer\"},\"protocols\":{\"items\":{\"enum\":[\"openai\",\"anthropic\",\"responses\"],\"type\":\"string\"},\"type\":\"array\"},\"provider_type\":{\"enum\":[\"openai\",\"anthropic\",\"custom\"],\"type\":\"string\"},\"proxy_url\":{\"type\":\"string\"},\"timeout_secs\":{\"maximum\":600,\"minimum\":1,\"type\":\"integer\"},\"website_url\":{\"type\":\"string\"},\"weight\":{\"format\":\"double\",\"type\":\"number\"}},\"required\":[\"name\",\"api_key\",\"models\"],\"type\":\"object\"},\"component_CreateProvidersBatchRequest\":{\"description\":\"shared 为单个创建请求的扁平展开(name 为基础名,自动编号 -01..-N;\\nshared.api_key 被忽略,逐 key 覆盖)+ api_keys。\\n\",\"properties\":{\"anthropic_base_url\":{\"type\":\"string\"},\"api_keys\":{\"description\":\"1..=100 个非空 key;批内重复归入失败组\",\"items\":{\"minLength\":1,\"type\":\"string\"},\"maxItems\":100,\"minItems\":1,\"type\":\"array\"},\"channel_kind\":{\"type\":\"string\"},\"default_protocol\":{\"enum\":[\"openai\",\"anthropic\"],\"type\":\"string\"},\"max_retries\":{\"maximum\":10,\"minimum\":0,\"type\":\"integer\"},\"model_mapping\":{\"additionalProperties\":{\"type\":\"string\"},\"type\":\"object\"},\"models\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"},\"name\":{\"type\":\"string\"},\"note\":{\"type\":\"string\"},\"openai_base_url\":{\"type\":\"string\"},\"priority\":{\"maximum\":100,\"minimum\":0,\"type\":\"integer\"},\"protocols\":{\"items\":{\"enum\":[\"openai\",\"anthropic\",\"responses\"],\"type\":\"string\"},\"type\":\"array\"},\"provider_type\":{\"enum\":[\"openai\",\"anthropic\",\"custom\"],\"type\":\"string\"},\"proxy_url\":{\"type\":\"string\"},\"timeout_secs\":{\"maximum\":600,\"minimum\":1,\"type\":\"integer\"},\"website_url\":{\"type\":\"string\"},\"weight\":{\"format\":\"double\",\"type\":\"number\"}},\"required\":[\"name\",\"models\",\"api_keys\"],\"type\":\"object\"},\"component_CreateRedemptionCodesRequest\":{\"properties\":{\"amount\":{\"description\":\"单张面值(元),≤1,000,000 且 ≥1 微元\",\"format\":\"double\",\"type\":\"number\"},\"batch\":{\"description\":\"批次名(必填,trim 后非空)\",\"maxLength\":64,\"type\":\"string\"},\"count\":{\"maximum\":500,\"minimum\":1,\"type\":\"integer\"},\"expires_at\":{\"description\":\"只接受 'YYYY-MM-DD'(补 23:59:59)或 'YYYY-MM-DD HH:MM:SS'(UTC);\\n拒绝 RFC3339 'T' 格式(与 SQLite datetime 字符串比较语义有关)。\\n空/缺省 = 永不过期。\\n\",\"type\":\"string\"},\"note\":{\"type\":\"string\"}},\"required\":[\"count\",\"amount\",\"batch\"],\"type\":\"object\"},\"component_CreateUserRequest\":{\"properties\":{\"display_name\":{\"maxLength\":64,\"type\":\"string\"},\"password\":{\"description\":\"8-256 字节,非全空白\",\"type\":\"string\"},\"role\":{\"default\":\"user\",\"enum\":[\"admin\",\"user\"],\"type\":\"string\"},\"username\":{\"description\":\"trim 后 1-64 字符\",\"maxLength\":64,\"minLength\":1,\"type\":\"string\"}},\"required\":[\"username\",\"password\"],\"type\":\"object\"},\"component_FetchModelsRequest\":{\"description\":\"编辑态回退:api_key / proxy_url 留空且带 provider_id 时取库存值\",\"properties\":{\"api_key\":{\"type\":\"string\"},\"base_url\":{\"type\":\"string\"},\"channel_kind\":{\"type\":\"string\"},\"protocol\":{\"enum\":[\"openai\",\"anthropic\"],\"type\":\"string\"},\"provider_id\":{\"description\":\"编辑已有渠道时传入,用于回退库存密钥/代理\",\"type\":\"string\"},\"provider_type\":{\"description\":\"旧客户端回退字段,protocol 缺省时使用\",\"type\":\"string\"},\"proxy_url\":{\"type\":\"string\"}},\"required\":[\"base_url\"],\"type\":\"object\"},\"component_LoginRequest\":{\"properties\":{\"password\":{\"type\":\"string\"},\"username\":{\"type\":\"string\"}},\"required\":[\"username\",\"password\"],\"type\":\"object\"},\"component_ModelTestRequest\":{\"properties\":{\"message\":{\"description\":\"单条用户消息(缺省 'Say OK');messages 提供时被覆盖\",\"type\":\"string\"},\"messages\":{\"items\":{\"properties\":{\"content\":{\"type\":\"string\"},\"role\":{\"type\":\"string\"}},\"required\":[\"role\",\"content\"],\"type\":\"object\"},\"type\":\"array\"},\"model\":{\"type\":\"string\"},\"protocol\":{\"description\":\"必须是渠道已声明协议,否则 400 unsupported_protocol;缺省用渠道默认\",\"enum\":[\"openai\",\"anthropic\",\"responses\"],\"type\":\"string\"},\"stream\":{\"default\":false,\"description\":\"true 走 SSE 路径(text/event-stream)\",\"type\":\"boolean\"}},\"required\":[\"model\"],\"type\":\"object\"},\"component_RedeemRequest\":{\"properties\":{\"code\":{\"description\":\"输入允许小写/多空格;服务端去空白 + 转大写\",\"type\":\"string\"}},\"required\":[\"code\"],\"type\":\"object\"},\"component_UpdateApiKeyRequest\":{\"description\":\"部分更新;expires_at 传空串 = 清除过期时间;无字段 400\",\"minProperties\":1,\"properties\":{\"expires_at\":{\"description\":\"未来时间(解析口径同创建:RFC3339 或宽松格式);**空串 = 清除**\\n(永不过期)。不加 format: date-time 的理由见创建请求同字段。\\n\",\"type\":\"string\"},\"is_active\":{\"type\":\"boolean\"},\"max_concurrent\":{\"format\":\"int64\",\"minimum\":0,\"type\":\"integer\"},\"models\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"},\"name\":{\"type\":\"string\"},\"quota_daily_tokens\":{\"format\":\"int64\",\"minimum\":0,\"type\":\"integer\"},\"rate_limit_rpm\":{\"format\":\"int64\",\"minimum\":0,\"type\":\"integer\"}},\"type\":\"object\"},\"component_UpdateProviderRequest\":{\"description\":\"部分更新;跨字段约束按\\\"库存值 + 本次改动\\\"的最终状态校验\\n(防止造出必失败渠道)。proxy_url 传空串 = 清除代理。\\n无任何字段 → 400 no_fields_to_update。\\n\",\"minProperties\":1,\"properties\":{\"anthropic_base_url\":{\"type\":\"string\"},\"api_key\":{\"description\":\"换 key 时排除自身做重复检测,冲突 409\",\"type\":\"string\"},\"channel_kind\":{\"type\":\"string\"},\"default_protocol\":{\"enum\":[\"openai\",\"anthropic\"],\"type\":\"string\"},\"is_active\":{\"description\":\"true 顺带清自动禁用状态(disabled_reason、consecutive_failures)\",\"type\":\"boolean\"},\"max_retries\":{\"maximum\":10,\"minimum\":0,\"type\":\"integer\"},\"model_mapping\":{\"additionalProperties\":{\"type\":\"string\"},\"type\":\"object\"},\"models\":{\"items\":{\"type\":\"string\"},\"type\":\"array\"},\"name\":{\"type\":\"string\"},\"note\":{\"type\":\"string\"},\"openai_base_url\":{\"type\":\"string\"},\"priority\":{\"maximum\":100,\"minimum\":0,\"type\":\"integer\"},\"protocols\":{\"items\":{\"enum\":[\"openai\",\"anthropic\",\"responses\"],\"type\":\"string\"},\"type\":\"array\"},\"provider_type\":{\"enum\":[\"openai\",\"anthropic\",\"custom\"],\"type\":\"string\"},\"proxy_url\":{\"description\":\"空串 = 清除代理\",\"type\":\"string\"},\"timeout_secs\":{\"maximum\":600,\"minimum\":1,\"type\":\"integer\"},\"website_url\":{\"type\":\"string\"},\"weight\":{\"format\":\"double\",\"type\":\"number\"}},\"type\":\"object\"},\"component_UpdateUserRequest\":{\"description\":\"部分更新;空对象(无任何字段)→ 400 no_fields_to_update\",\"minProperties\":1,\"properties\":{\"display_name\":{\"maxLength\":64,\"type\":\"string\"},\"is_active\":{\"type\":\"boolean\"},\"password\":{\"description\":\"改密后 token_version+1,该用户已签发 JWT 全部失效\",\"type\":\"string\"},\"role\":{\"enum\":[\"admin\",\"user\"],\"type\":\"string\"}},\"type\":\"object\"},\"component_UpsertPriceRequest\":{\"description\":\"创建:model 必填;价格缺省为 0。\\n更新:部分语义;cached_price 三态 —— 字段缺省 = 不改动,\\n显式 null = 清回 NULL,数值 = 更新(optional + nullable 双层表达)。\\n\",\"properties\":{\"cached_price\":{\"description\":\"三态字段(缺省/显式 null/数值),见 schema 描述\",\"format\":\"double\",\"minimum\":0,\"type\":[\"number\",\"null\"]},\"completion_price\":{\"format\":\"double\",\"minimum\":0,\"type\":\"number\"},\"model\":{\"type\":\"string\"},\"prompt_price\":{\"format\":\"double\",\"minimum\":0,\"type\":\"number\"}},\"type\":\"object\"}},\"targets\":{\"v0\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_LoginRequest\"},\"v1\":{\"type\":\"string\"},\"v10\":{\"type\":\"string\"},\"v11\":{\"default\":50,\"type\":\"integer\"},\"v12\":{\"default\":0,\"minimum\":0,\"type\":\"integer\"},\"v13\":{\"type\":\"string\"},\"v14\":{\"enum\":[\"0\",\"1\"],\"type\":\"string\"},\"v15\":{\"type\":\"string\"},\"v16\":{\"type\":\"string\"},\"v17\":{\"default\":50,\"type\":\"integer\"},\"v18\":{\"default\":0,\"minimum\":0,\"type\":\"integer\"},\"v19\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_RedeemRequest\"},\"v2\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_CreateApiKeyRequest\"},\"v20\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_UpdateApiKeyRequest\"},\"v21\":{\"type\":\"string\"},\"v22\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_AdjustBalanceRequest\"},\"v23\":{\"type\":\"string\"},\"v24\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_UpsertPriceRequest\"},\"v25\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_CreateProviderRequest\"},\"v26\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_CreateProvidersBatchRequest\"},\"v27\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_CreateRedemptionCodesRequest\"},\"v28\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_CreateUserRequest\"},\"v29\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_BatchCreateUsersRequest\"},\"v3\":{\"type\":\"string\"},\"v30\":{\"type\":\"string\"},\"v31\":{\"type\":\"string\"},\"v32\":{\"type\":\"string\"},\"v33\":{\"type\":\"string\"},\"v34\":{\"type\":\"string\"},\"v35\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_FetchModelsRequest\"},\"v36\":{\"type\":\"string\"},\"v37\":{\"type\":\"string\"},\"v38\":{\"default\":50,\"type\":\"integer\"},\"v39\":{\"default\":0,\"minimum\":0,\"type\":\"integer\"},\"v4\":{\"type\":\"string\"},\"v40\":{\"type\":\"string\"},\"v41\":{\"default\":50,\"type\":\"integer\"},\"v42\":{\"default\":0,\"minimum\":0,\"type\":\"integer\"},\"v43\":{\"type\":\"string\"},\"v44\":{\"default\":50,\"type\":\"integer\"},\"v45\":{\"default\":0,\"minimum\":0,\"type\":\"integer\"},\"v46\":{\"type\":\"string\"},\"v47\":{\"default\":50,\"type\":\"integer\"},\"v48\":{\"default\":0,\"minimum\":0,\"type\":\"integer\"},\"v49\":{\"default\":50,\"type\":\"integer\"},\"v5\":{\"enum\":[\"0\",\"1\"],\"type\":\"string\"},\"v50\":{\"default\":0,\"minimum\":0,\"type\":\"integer\"},\"v51\":{\"type\":\"string\"},\"v52\":{\"enum\":[\"unused\",\"used\",\"disabled\",\"expired\"],\"type\":\"string\"},\"v53\":{\"default\":50,\"type\":\"integer\"},\"v54\":{\"default\":0,\"minimum\":0,\"type\":\"integer\"},\"v55\":{\"type\":\"string\"},\"v56\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_ModelTestRequest\"},\"v57\":{\"type\":\"string\"},\"v58\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_UpsertPriceRequest\"},\"v59\":{\"type\":\"string\"},\"v6\":{\"type\":\"string\"},\"v60\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_UpdateProviderRequest\"},\"v61\":{\"type\":\"string\"},\"v62\":{\"$ref\":\"urn:openapi-to-rust:request-validation#/$defs/components/component_UpdateUserRequest\"},\"v63\":{\"type\":\"string\"},\"v7\":{\"type\":\"string\"},\"v8\":{\"default\":50,\"type\":\"integer\"},\"v9\":{\"default\":0,\"minimum\":0,\"type\":\"integer\"}}},\"$id\":\"urn:openapi-to-rust:request-validation\",\"$schema\":\"https://json-schema.org/draft/2020-12/schema\"}";
const MAX_VALIDATION_ERRORS: usize = 16usize;
pub(crate) const VALIDATION_TARGET_0_BODY: &str = "#/$defs/targets/v0";
pub(crate) const VALIDATION_TARGET_1_PATH_0: &str = "#/$defs/targets/v1";
pub(crate) const VALIDATION_TARGET_2_BODY: &str = "#/$defs/targets/v2";
pub(crate) const VALIDATION_TARGET_3_PATH_0: &str = "#/$defs/targets/v3";
pub(crate) const VALIDATION_TARGET_4_QUERY_0: &str = "#/$defs/targets/v4";
pub(crate) const VALIDATION_TARGET_5_QUERY_1: &str = "#/$defs/targets/v5";
pub(crate) const VALIDATION_TARGET_6_QUERY_2: &str = "#/$defs/targets/v6";
pub(crate) const VALIDATION_TARGET_7_QUERY_3: &str = "#/$defs/targets/v7";
pub(crate) const VALIDATION_TARGET_8_QUERY_0: &str = "#/$defs/targets/v8";
pub(crate) const VALIDATION_TARGET_9_QUERY_1: &str = "#/$defs/targets/v9";
pub(crate) const VALIDATION_TARGET_10_QUERY_2: &str = "#/$defs/targets/v10";
pub(crate) const VALIDATION_TARGET_11_QUERY_0: &str = "#/$defs/targets/v11";
pub(crate) const VALIDATION_TARGET_12_QUERY_1: &str = "#/$defs/targets/v12";
pub(crate) const VALIDATION_TARGET_13_QUERY_2: &str = "#/$defs/targets/v13";
pub(crate) const VALIDATION_TARGET_14_QUERY_3: &str = "#/$defs/targets/v14";
pub(crate) const VALIDATION_TARGET_15_QUERY_4: &str = "#/$defs/targets/v15";
pub(crate) const VALIDATION_TARGET_16_QUERY_5: &str = "#/$defs/targets/v16";
pub(crate) const VALIDATION_TARGET_17_QUERY_0: &str = "#/$defs/targets/v17";
pub(crate) const VALIDATION_TARGET_18_QUERY_1: &str = "#/$defs/targets/v18";
pub(crate) const VALIDATION_TARGET_19_BODY: &str = "#/$defs/targets/v19";
pub(crate) const VALIDATION_TARGET_20_BODY: &str = "#/$defs/targets/v20";
pub(crate) const VALIDATION_TARGET_21_PATH_0: &str = "#/$defs/targets/v21";
pub(crate) const VALIDATION_TARGET_22_BODY: &str = "#/$defs/targets/v22";
pub(crate) const VALIDATION_TARGET_23_PATH_0: &str = "#/$defs/targets/v23";
pub(crate) const VALIDATION_TARGET_24_BODY: &str = "#/$defs/targets/v24";
pub(crate) const VALIDATION_TARGET_25_BODY: &str = "#/$defs/targets/v25";
pub(crate) const VALIDATION_TARGET_26_BODY: &str = "#/$defs/targets/v26";
pub(crate) const VALIDATION_TARGET_27_BODY: &str = "#/$defs/targets/v27";
pub(crate) const VALIDATION_TARGET_28_BODY: &str = "#/$defs/targets/v28";
pub(crate) const VALIDATION_TARGET_29_BODY: &str = "#/$defs/targets/v29";
pub(crate) const VALIDATION_TARGET_30_PATH_0: &str = "#/$defs/targets/v30";
pub(crate) const VALIDATION_TARGET_31_PATH_0: &str = "#/$defs/targets/v31";
pub(crate) const VALIDATION_TARGET_32_PATH_0: &str = "#/$defs/targets/v32";
pub(crate) const VALIDATION_TARGET_33_PATH_0: &str = "#/$defs/targets/v33";
pub(crate) const VALIDATION_TARGET_34_PATH_0: &str = "#/$defs/targets/v34";
pub(crate) const VALIDATION_TARGET_35_BODY: &str = "#/$defs/targets/v35";
pub(crate) const VALIDATION_TARGET_36_PATH_0: &str = "#/$defs/targets/v36";
pub(crate) const VALIDATION_TARGET_37_PATH_0: &str = "#/$defs/targets/v37";
pub(crate) const VALIDATION_TARGET_38_QUERY_0: &str = "#/$defs/targets/v38";
pub(crate) const VALIDATION_TARGET_39_QUERY_1: &str = "#/$defs/targets/v39";
pub(crate) const VALIDATION_TARGET_40_QUERY_2: &str = "#/$defs/targets/v40";
pub(crate) const VALIDATION_TARGET_41_QUERY_0: &str = "#/$defs/targets/v41";
pub(crate) const VALIDATION_TARGET_42_QUERY_1: &str = "#/$defs/targets/v42";
pub(crate) const VALIDATION_TARGET_43_QUERY_2: &str = "#/$defs/targets/v43";
pub(crate) const VALIDATION_TARGET_44_QUERY_0: &str = "#/$defs/targets/v44";
pub(crate) const VALIDATION_TARGET_45_QUERY_1: &str = "#/$defs/targets/v45";
pub(crate) const VALIDATION_TARGET_46_QUERY_2: &str = "#/$defs/targets/v46";
pub(crate) const VALIDATION_TARGET_47_QUERY_0: &str = "#/$defs/targets/v47";
pub(crate) const VALIDATION_TARGET_48_QUERY_1: &str = "#/$defs/targets/v48";
pub(crate) const VALIDATION_TARGET_49_QUERY_0: &str = "#/$defs/targets/v49";
pub(crate) const VALIDATION_TARGET_50_QUERY_1: &str = "#/$defs/targets/v50";
pub(crate) const VALIDATION_TARGET_51_QUERY_2: &str = "#/$defs/targets/v51";
pub(crate) const VALIDATION_TARGET_52_QUERY_3: &str = "#/$defs/targets/v52";
pub(crate) const VALIDATION_TARGET_53_QUERY_0: &str = "#/$defs/targets/v53";
pub(crate) const VALIDATION_TARGET_54_QUERY_1: &str = "#/$defs/targets/v54";
pub(crate) const VALIDATION_TARGET_55_PATH_0: &str = "#/$defs/targets/v55";
pub(crate) const VALIDATION_TARGET_56_BODY: &str = "#/$defs/targets/v56";
pub(crate) const VALIDATION_TARGET_57_PATH_0: &str = "#/$defs/targets/v57";
pub(crate) const VALIDATION_TARGET_58_BODY: &str = "#/$defs/targets/v58";
pub(crate) const VALIDATION_TARGET_59_PATH_0: &str = "#/$defs/targets/v59";
pub(crate) const VALIDATION_TARGET_60_BODY: &str = "#/$defs/targets/v60";
pub(crate) const VALIDATION_TARGET_61_PATH_0: &str = "#/$defs/targets/v61";
pub(crate) const VALIDATION_TARGET_62_BODY: &str = "#/$defs/targets/v62";
pub(crate) const VALIDATION_TARGET_63_PATH_0: &str = "#/$defs/targets/v63";
static VALIDATORS: ::std::sync::LazyLock<::std::result::Result<::jsonschema::ValidatorMap, ()>> =
    ::std::sync::LazyLock::new(|| {
        let schema: ::serde_json::Value =
            ::serde_json::from_str(VALIDATION_SCHEMA).map_err(|_| ())?;
        ::jsonschema::options()
            .with_draft(::jsonschema::Draft::Draft202012)
            .should_validate_formats(true)
            .with_pattern_options(::jsonschema::PatternOptions::regex())
            .build_map(&schema)
            .map_err(|_| ())
    });
pub(crate) async fn decode_json_body<T>(
    request: ::axum::extract::Request,
    target: &str,
    expected_media_type: &str,
    required: bool,
    max_body_bytes: usize,
) -> ::std::result::Result<::std::option::Option<T>, RequestValidationRejection>
where
    T: ::serde::de::DeserializeOwned,
{
    let (parts, body) = request.into_parts();
    let content_type = parts
        .headers
        .get(::axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let is_json = content_type.is_some_and(|value| media_type_is(value, expected_media_type));
    if !is_json {
        if content_type.is_none() && !required {
            let bytes = read_body(body, max_body_bytes).await?;
            if bytes.is_empty() {
                return Ok(None);
            }
        }
        return Err(unsupported_media_type());
    }
    let bytes = read_body(body, max_body_bytes).await?;
    if bytes.is_empty() {
        return if required {
            Err(malformed_request())
        } else {
            Ok(None)
        };
    }
    let instance: ::serde_json::Value =
        ::serde_json::from_slice(&bytes).map_err(|_| malformed_request())?;
    validate(target, "/body", &instance)?;
    let typed = ::serde_json::from_value(instance).map_err(|_| generated_contract_error())?;
    Ok(Some(typed))
}
pub(crate) fn decode_parameter<T>(
    raw: &str,
    target: &str,
    location: &str,
    string_wire: bool,
) -> ::std::result::Result<T, RequestValidationRejection>
where
    T: ::serde::de::DeserializeOwned + ::serde::Serialize,
{
    let typed = if string_wire {
        let instance = ::serde_json::Value::String(raw.to_string());
        validate(target, location, &instance)?;
        ::serde_json::from_value(instance).map_err(|_| generated_contract_error())?
    } else {
        ::serde_json::from_value(::serde_json::Value::String(raw.to_string()))
            .or_else(|_| ::serde_json::from_str(raw))
            .map_err(|_| malformed_parameter(location))?
    };
    validate_parameter(target, location, &typed)?;
    Ok(typed)
}
pub(crate) fn validate_string_parameter(
    target: &str,
    location: &str,
    raw: &str,
) -> ::std::result::Result<(), RequestValidationRejection> {
    validate(
        target,
        location,
        &::serde_json::Value::String(raw.to_string()),
    )
}
pub(crate) fn validate_parameter<T>(
    target: &str,
    location: &str,
    value: &T,
) -> ::std::result::Result<(), RequestValidationRejection>
where
    T: ::serde::Serialize + ?Sized,
{
    let instance = ::serde_json::to_value(value).map_err(|_| generated_contract_error())?;
    validate(target, location, &instance)
}
pub(crate) fn parse_cookies(
    headers: &::axum::http::HeaderMap,
) -> ::std::result::Result<::std::collections::BTreeMap<String, String>, RequestValidationRejection>
{
    let mut cookies = ::std::collections::BTreeMap::new();
    for header in headers.get_all(::axum::http::header::COOKIE).iter() {
        let line = header
            .to_str()
            .map_err(|_| malformed_parameter("/cookie"))?;
        for field in line.split(';') {
            let field = field.trim();
            if field.is_empty() {
                continue;
            }
            let (name, value) = field
                .split_once('=')
                .ok_or_else(|| malformed_parameter("/cookie"))?;
            let name = name.trim();
            if name.is_empty()
                || cookies
                    .insert(name.to_string(), value.to_string())
                    .is_some()
            {
                return Err(malformed_parameter("/cookie"));
            }
        }
    }
    Ok(cookies)
}
async fn read_body(
    body: ::axum::body::Body,
    max_body_bytes: usize,
) -> ::std::result::Result<::axum::body::Bytes, RequestValidationRejection> {
    ::axum::body::to_bytes(body, max_body_bytes)
        .await
        .map_err(|error| {
            let source = ::std::error::Error::source(&error);
            if source.is_some_and(|source| source.is::<::http_body_util::LengthLimitError>()) {
                request_body_too_large()
            } else {
                malformed_request()
            }
        })
}
fn media_type_is(content_type: &str, expected: &str) -> bool {
    let Ok(content_type) = content_type.parse::<::mime::Mime>() else {
        return false;
    };
    let Ok(expected) = expected.parse::<::mime::Mime>() else {
        return false;
    };
    content_type.type_() == expected.type_()
        && content_type.subtype() == expected.subtype()
        && content_type.suffix() == expected.suffix()
        && expected.params().all(|(name, value)| {
            content_type
                .get_param(name)
                .is_some_and(|actual| actual == value)
        })
}
pub(crate) fn validate(
    target: &str,
    location: &str,
    instance: &::serde_json::Value,
) -> ::std::result::Result<(), RequestValidationRejection> {
    let validators = VALIDATORS
        .as_ref()
        .map_err(|_| generated_contract_error())?;
    let Some(validator) = validators.get(target) else {
        return Err(generated_contract_error());
    };
    let mut errors: ::std::vec::Vec<InvalidParameter> = ::std::vec::Vec::new();
    for error in validator.iter_errors(instance) {
        let keyword = error.kind().keyword();
        let (code, message) = public_violation(keyword);
        let mut pointer = format!("{location}{}", error.instance_path());
        if let ::jsonschema::error::ValidationErrorKind::Required { property } = error.kind() {
            if let Some(property) = property.as_str() {
                pointer.push('/');
                pointer.push_str(&escape_pointer_token(property));
            }
        }
        let violation = InvalidParameter {
            code: code.to_string(),
            location: pointer,
            message: message.to_string(),
        };
        if !errors.contains(&violation) {
            errors.push(violation);
            if errors.len() == MAX_VALIDATION_ERRORS {
                break;
            }
        }
    }
    errors.sort_by(|left, right| {
        (&left.location, &left.code, &left.message).cmp(&(
            &right.location,
            &right.code,
            &right.message,
        ))
    });
    if errors.is_empty() {
        return Ok(());
    }
    Err(RequestValidationRejection(ProblemDetails {
        r#type: "https://openapi-to-rust.dev/problems/validation".to_string(),
        title: "Request validation failed".to_string(),
        status: 422,
        code: "request_validation_failed".to_string(),
        errors,
    }))
}
pub(crate) fn malformed_request() -> RequestValidationRejection {
    public_problem(
        400,
        "https://openapi-to-rust.dev/problems/malformed-request",
        "Malformed request",
        "malformed_request",
    )
}
pub(crate) fn malformed_parameter(location: &str) -> RequestValidationRejection {
    parameter_problem(
        400,
        "https://openapi-to-rust.dev/problems/malformed-parameter",
        "Malformed request parameter",
        "malformed_parameter",
        "malformed",
        location,
        "is malformed",
    )
}
pub(crate) fn missing_parameter(location: &str) -> RequestValidationRejection {
    parameter_problem(
        422,
        "https://openapi-to-rust.dev/problems/validation",
        "Request validation failed",
        "request_validation_failed",
        "required",
        location,
        "is required",
    )
}
fn schema_parameter_problem(
    location: &str,
    code: &str,
    message: &str,
) -> RequestValidationRejection {
    parameter_problem(
        422,
        "https://openapi-to-rust.dev/problems/validation",
        "Request validation failed",
        "request_validation_failed",
        code,
        location,
        message,
    )
}
fn parameter_problem(
    status: u16,
    problem_type: &str,
    title: &str,
    code: &str,
    error_code: &str,
    location: &str,
    message: &str,
) -> RequestValidationRejection {
    RequestValidationRejection(ProblemDetails {
        r#type: problem_type.to_string(),
        title: title.to_string(),
        status,
        code: code.to_string(),
        errors: vec![InvalidParameter {
            code: error_code.to_string(),
            location: location.to_string(),
            message: message.to_string(),
        }],
    })
}
pub(crate) fn request_body_too_large() -> RequestValidationRejection {
    public_problem(
        413,
        "https://openapi-to-rust.dev/problems/request-body-too-large",
        "Request body too large",
        "request_body_too_large",
    )
}
pub(crate) fn unsupported_media_type() -> RequestValidationRejection {
    public_problem(
        415,
        "https://openapi-to-rust.dev/problems/unsupported-media-type",
        "Unsupported media type",
        "unsupported_media_type",
    )
}
pub(crate) fn generated_contract_error() -> RequestValidationRejection {
    public_problem(
        500,
        "https://openapi-to-rust.dev/problems/generated-contract-error",
        "Internal server error",
        "generated_contract_error",
    )
}
fn public_problem(
    status: u16,
    problem_type: &str,
    title: &str,
    code: &str,
) -> RequestValidationRejection {
    RequestValidationRejection(ProblemDetails {
        r#type: problem_type.to_string(),
        title: title.to_string(),
        status,
        code: code.to_string(),
        errors: Vec::new(),
    })
}
fn public_violation(keyword: &str) -> (&'static str, &'static str) {
    match keyword {
        "required" => ("required", "is required"),
        "type" => ("type", "has an invalid type"),
        "enum" => ("enum", "has an unsupported value"),
        "const" => ("const", "has an unsupported value"),
        "format" => ("format", "has an invalid format"),
        "pattern" => ("pattern", "does not match the required format"),
        "minLength" => ("min_length", "does not meet the length constraint"),
        "maxLength" => ("max_length", "does not meet the length constraint"),
        "minimum" => ("minimum", "is outside the allowed range"),
        "maximum" => ("maximum", "is outside the allowed range"),
        "exclusiveMinimum" => ("exclusive_minimum", "is outside the allowed range"),
        "exclusiveMaximum" => ("exclusive_maximum", "is outside the allowed range"),
        "multipleOf" => ("multiple_of", "is outside the allowed range"),
        "minItems" => ("min_items", "does not meet the item-count constraint"),
        "maxItems" => ("max_items", "does not meet the item-count constraint"),
        "uniqueItems" => ("unique_items", "contains duplicate items"),
        "minProperties" => (
            "min_properties",
            "does not meet the property-count constraint",
        ),
        "maxProperties" => (
            "max_properties",
            "does not meet the property-count constraint",
        ),
        "additionalProperties" => ("additional_properties", "contains unsupported properties"),
        "unevaluatedProperties" => ("unevaluated_properties", "contains unsupported properties"),
        "anyOf" => ("any_of", "does not match the required shape"),
        "oneOf" => ("one_of", "does not match the required shape"),
        "not" => ("not", "does not match the required shape"),
        _ => ("invalid_value", "is invalid"),
    }
}
fn escape_pointer_token(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}
