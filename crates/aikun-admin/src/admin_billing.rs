//! Admin tag · billing 方法组:模型价格 CRUD / 余额调账与流水 / 兑换码
//! (生成/列表/禁用)。业务逻辑迁自 `aikun/src/api/admin/billing.rs` 与
//! `aikun/src/api/admin/redemption.rs`(兑换码管理端三 op),对齐
//! `openapi.yaml` 新契约:`{items,total}` 信封 + 分页、删除 204、调账 201、
//! problem+json、disable 动作端点转 DELETE。
//!
//! # 装配契约(main.rs 侧,见 docs/migration-playbook.md)
//!
//! * **过渡期 DB/rand/sha2 走端口 trait**:`BillingStore` 由二进制装配层
//!   (main.rs 的适配器,参照 Auth 批的 `DbAuthStore`)实现;aikun-admin
//!   不依赖存储引擎/rand/sha2/uuid,依赖纯净度约束见 playbook 踩坑 #7。
//!   明文码生成(熵源)、sha256、行 ID 都经端口注入;**明文只在
//!   `admin_create_redemption_codes` 的响应里出现一次**,库中只落哈希与
//!   后缀——这是本组最重要的安全语义,永不出列表。
//! * **lib.rs 的 `AdminService.billing` 字段是具体类型**(路由工厂只收一个
//!   `AdminApi` 实现者),所以本服务用 `Arc<dyn BillingStore>` 而非泛型,
//!   与 auth.rs 的 `AuthService<S>` 不同——这是 facade 约束下的取舍。
//! * 请求体/查询参数校验在生成路由层(jsonschema 运行时):count 越界、
//!   负价格、非枚举 status 到不了 impl(422 request_validation_failed);
//!   impl 只保留 schema 表达不了的校验(trim 非空、通配形状、金额上限、
//!   微元下限、expires_at 形状)。
//! * `in_use` 刻意保持宽容语义(api-mapping 备注 9):非 '1' 值静默忽略
//!   等同缺省,不 400。
//!
//! # 已知 spec 缺口(记 notes 待裁决,别当定案)
//!
//! 旧实现的 5xx 分支(query_failed / internal_error / create_failed /
//! update_failed / delete_failed)在本组 9 个响应枚举里只有
//! `AdminCreateRedemptionCodesResponse` 有 500 变体,其余 8 个都缺
//! (spec 的 Problem 词表里有这些 500 码,却只在 providers/model-health/
//! redemption-codes 三个 op 挂了 500 响应)。**过渡期映射**(HTTP 状态按
//! 枚举可得变体妥协,body `code` 保留真实短码、`status` 与 HTTP 一致,
//! RFC 9457 自洽;补 500 变体需 spec 改动后重生成;全管理面各批的
//! 汇总对照见 `docs/api-mapping.md` 备注 13,CHANGELOG 已单列):
//!
//! | op | 旧 5xx 码 | 过渡期变体 |
//! |---|---|---|
//! | adminListPrices | internal_error / query_failed | 403 Forbidden |
//! | adminListBillingTransactions | internal_error / query_failed | 403 Forbidden |
//! | adminListRedemptionCodes | internal_error / query_failed | 403 Forbidden |
//! | adminCreatePrice | internal_error / create_failed / query_failed | 400 BadRequest |
//! | adminUpdatePrice | internal_error / update_failed / query_failed | 400 BadRequest |
//! | adminDeletePrice | internal_error / delete_failed | 403 Forbidden |
//! | adminAdjustUserBalance | internal_error / update_failed | 400 BadRequest |
//! | adminDisableRedemptionCode | internal_error / update_failed | 403 Forbidden |
//! | adminCreateRedemptionCodes | code_collision / create_failed / internal_error | 500(枚举有) |

use std::collections::HashSet;
use std::sync::Arc;

use aikun_openapi::openapi::server::api::AdminListRedemptionCodesStatus;
use aikun_openapi::openapi::server::errors::*;
use aikun_openapi::openapi::types::*;

use crate::problem;

// ---------- 价格表缓存失效(roadmap 阶段 3) ----------
//
// 二进制 billing 的价格表缓存每次取用比对「本世代 + 存储层表令牌」;
// 价格 CRUD 成功后递增世代,保证「改价后下一笔计费立即用新价」。

static PRICE_TABLE_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// 价格写路径成功后调用:令价格表缓存下次取用时重载。
pub fn bump_price_table_epoch() {
    PRICE_TABLE_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// 当前世代(价格表缓存比对用;只增不减)。
pub fn price_table_epoch() -> u64 {
    PRICE_TABLE_EPOCH.load(std::sync::atomic::Ordering::Acquire)
}

// ---------- 过渡期存储端口 ----------
//
// aikun-store 就位前(plan 步骤 8)DB/rand/sha2/uuid 由装配层适配器实现本
// 端口;业务编排(校验顺序、通配匹配、三态 PATCH、分页 clamp、状态码映射)
// 全在本 crate。适配器行为契约写在各方法文档,SQL 语义迁自旧 handler。

/// 端口错误。`Failed` 携带的内容只进 tracing 日志,不出 problem+json
/// (与旧实现一致:对外只给短码)。
#[derive(Debug)]
pub enum BillingStoreError {
    /// 连接/锁不可用(旧 500 internal_error)。
    Unavailable,
    /// SQL 执行失败(旧 500 query_failed / create_failed / ...)。
    Failed(String),
    /// 唯一约束冲突:价格 model 撞唯一键(409)、兑换码 code_hash 碰撞
    /// (500 code_collision)。
    Constraint,
}

/// 价格条目行(库存原文;时间仍是 SQLite datetime 字符串,出参转换)。
pub struct PriceRecord {
    pub id: String,
    pub model: String,
    pub prompt_price: f64,
    pub completion_price: f64,
    pub cached_price: Option<f64>,
    pub created_at: String,
    pub updated_at: String,
}

/// 余额流水行(username 为 LEFT JOIN users 的兑换者名,用户已删除为 None)。
pub struct TxRecord {
    pub id: String,
    pub user_id: String,
    pub username: Option<String>,
    pub amount_micro: i64,
    pub balance_after_micro: i64,
    /// 'recharge' | 'adjust'(库内 CHECK 约束保证)
    pub kind: String,
    pub note: String,
    pub created_at: String,
}

/// 兑换码列表行(永不含明文/哈希;expired 为库内时间比较的计算态)。
pub struct RedemptionCodeRecord {
    pub id: String,
    /// 掩码后缀(明文末 4 位)
    pub code_suffix: String,
    pub amount_micro: i64,
    pub batch: String,
    /// 'unused' | 'used' | 'disabled'(库内 CHECK 约束保证)
    pub status: String,
    pub used_by_username: Option<String>,
    pub used_at: Option<String>,
    pub expires_at: Option<String>,
    pub note: String,
    pub created_at: String,
    /// expires_at 非空且 <= datetime('now')(SQL 计算,与 expired 筛选同口径)
    pub expired: bool,
}

/// 待落库的兑换码行(哈希与后缀由 impl 从明文算好传入;id 装配层生成)。
pub struct NewRedemptionCode {
    pub code_hash: String,
    pub code_suffix: String,
    pub amount_micro: i64,
    pub batch: String,
    /// None = 永不过期
    pub expires_at: Option<String>,
    pub note: String,
}

/// `disable_redemption_code` 的三态结果。
pub enum DisableOutcome {
    Disabled,
    /// 存在但非 unused(used / disabled)→ 409 not_unused
    NotUnused,
    /// 不存在 → 404
    NotFound,
}

/// billing 方法组过渡期端口(装配层实现,复用二进制的 DbPool/rand/sha2)。
/// 阶段 S async 化(docs/surreal-migration.md §六):端口随装配适配器
/// 翻转 `#[async_trait]`;纯计算方法(generate_code / hash_code)保持 sync。
#[async_trait::async_trait]
pub trait BillingStore: Send + Sync + 'static {
    /// 全量价格条目,ORDER BY model(旧 list_prices 的 SQL 序)。
    async fn all_prices(&self) -> Result<Vec<PriceRecord>, BillingStoreError>;

    /// 全部渠道(含禁用,只看"已添加")的模型名集合。查询失败按旧行为
    /// **静默吞掉、返回空集**(旧 channel_models 的 `if let Ok` 语义:
    /// in_use=1 在 providers 表读不出时给出空结果,而不是 5xx)。
    async fn all_channel_models(&self) -> HashSet<String>;

    /// 插入价格行(id 生成、created_at/updated_at 缺省、写后回读整行;
    /// model 撞唯一键 → `Constraint`)。
    async fn insert_price(
        &self,
        model: &str,
        prompt_price: f64,
        completion_price: f64,
        cached_price: Option<f64>,
    ) -> Result<PriceRecord, BillingStoreError>;

    /// 部分更新价格(None = 不改该列;cached_price 双层同请求体三态语义,
    /// 并置 updated_at)。返回更新后整行;行不存在 → `Ok(None)`。
    async fn update_price(
        &self,
        id: &str,
        model: Option<&str>,
        prompt_price: Option<f64>,
        completion_price: Option<f64>,
        cached_price: Option<Option<f64>>,
    ) -> Result<Option<PriceRecord>, BillingStoreError>;

    /// 删除价格;false = 行不存在(→ 404)。
    async fn delete_price(&self, id: &str) -> Result<bool, BillingStoreError>;

    /// 同事务 UPDATE users.balance + INSERT billing_transactions
    /// (余额变动与流水要么都成、要么都不成)。返回调后余额(微元);
    /// `Ok(None)` = 用户不存在(→ 404)。流水 id 装配层生成。
    async fn adjust_balance(
        &self,
        user_id: &str,
        amount_micro: i64,
        kind: &str,
        note: &str,
    ) -> Result<Option<i64>, BillingStoreError>;

    /// 流水分页:user_id 为 None/空 = 全部;ORDER BY created_at DESC,
    /// rowid DESC(旧序);返回 (total, 当前页)。
    async fn list_transactions(
        &self,
        user_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<TxRecord>), BillingStoreError>;

    /// 兑换码分页:batch 为 None/空 = 全部;status 只收
    /// unused/used/disabled/expired(生成层已枚举校验,expired 由装配层
    /// 翻译成 `status='unused' AND expires_at <= datetime('now')`);
    /// ORDER BY created_at DESC, rowid DESC。返回 (total, 当前页)。
    /// 永不返回明文/哈希。
    async fn list_redemption_codes(
        &self,
        batch: Option<&str>,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<RedemptionCodeRecord>), BillingStoreError>;

    /// 整批一个事务插入兑换码行(id 装配层生成):中途失败整体回滚,
    /// 避免哈希落库而明文从未送达;code_hash 碰撞 → `Constraint`。
    async fn insert_redemption_codes(
        &self,
        rows: &[NewRedemptionCode],
    ) -> Result<(), BillingStoreError>;

    /// `UPDATE ... SET status='disabled' WHERE id=? AND status='unused'`
    /// 的三态结果(影响行数 0 时回查存在性区分 NotUnused/NotFound)。
    async fn disable_redemption_code(&self, id: &str) -> Result<DisableOutcome, BillingStoreError>;

    /// 生成一张明文兑换码:`AK-XXXX-XXXX-XXXX-XXXX`(32 字符集
    /// ABCDEFGHJKLMNPQRSTUVWXYZ23456789,去易混淆 0/O/1/I,2^80 组合)。
    /// rand 不在本 crate 依赖面,熵源经端口注入(适配器复用旧
    /// `aikun/src/api/redemption.rs::generate_code` 的实现)。
    fn generate_code(&self) -> String;

    /// sha256(code) 的十六进制(适配器复用 sha2;与兑换核销同算法,
    /// 用户侧 redeem 在 user.rs 批次,共享同一哈希口径)。
    fn hash_code(&self, code: &str) -> String;
}

// ---------- 服务 ----------

/// billing 方法组服务。具体类型(lib.rs 的 `AdminService.billing` 字段
/// 约束),端口以 `Arc<dyn BillingStore>` 注入。
#[derive(Clone)]
pub struct AdminBillingService {
    store: Arc<dyn BillingStore>,
}

impl AdminBillingService {
    pub fn new(store: Arc<dyn BillingStore>) -> Self {
        Self { store }
    }

    // ---------- 校验/映射助手(迁自旧 handler,纯函数) ----------

    /// 元 → 微元,四舍五入(镜像 `aikun/src/billing/mod.rs::yuan_to_micro`,
    /// 依赖纯净度约束下本 crate 复制这 2 行,store 收编时统一)。
    fn yuan_to_micro(yuan: f64) -> i64 {
        (yuan * 1_000_000.0).round() as i64
    }

    /// 微元 → 元(镜像 `micro_to_yuan`;整除 1e6 在 f64 下精确)。
    fn micro_to_yuan(micro: i64) -> f64 {
        micro as f64 / 1_000_000.0
    }

    /// 分页大小:默认 50,clamp 1..=200(spec 全局语义 #3;越界静默截断,
    /// 刻意不 400——schema 不放 min/max 正是为此)。
    fn page_limit(limit: Option<i64>) -> i64 {
        limit.unwrap_or(50).clamp(1, 200)
    }

    /// 分页偏移:默认 0,防负(生成层 schema minimum:0 已拦,此处兜底
    /// 直接调用方)。
    fn page_offset(offset: Option<i64>) -> i64 {
        offset.unwrap_or(0).max(0)
    }

    /// 价格字段校验(迁自旧 valid_price_fields):model 非空(创建必填)、
    /// 通配只允许末尾单个 * 且前缀非空、价格有限非负。
    /// 注:负价格/NaN 现由生成层 schema(minimum: 0)拦成 422,这里的
    /// 有限/非负检查防御直接调用方,HTTP 路径上通常到不了。
    fn validate_price_fields(
        req: &UpsertPriceRequest,
        creating: bool,
    ) -> Result<(), (u16, &'static str, &'static str)> {
        let invalid_model_empty: (u16, &str, &str) = (400, "invalid_model", "model 不能为空");
        let invalid_model_wildcard: (u16, &str, &str) = (
            400,
            "invalid_model",
            "通配只支持末尾单个 *,且前缀非空(如 gpt-*)",
        );
        let model = req.model.as_deref().map(str::trim);
        if creating && model.unwrap_or("").is_empty() {
            return Err(invalid_model_empty);
        }
        if let Some(m) = model {
            if m.is_empty() {
                return Err(invalid_model_empty);
            }
            if m.contains('*') {
                // 通配只允许末尾单个 *:剥掉最后一个字符后,前缀不得再含 *
                // 且非空(裸 * 与 a** 均拒绝——"a**" 剥掉全部 * 的旧写法会
                // 误放行,这种条目永远不会被 find_price 命中,属配置垃圾)。
                if !m.ends_with('*') || m.len() < 2 || m[..m.len() - 1].contains('*') {
                    return Err(invalid_model_wildcard);
                }
            }
        }
        for v in [req.prompt_price, req.completion_price]
            .into_iter()
            .flatten()
            .chain(req.cached_price.into_iter().flatten())
        {
            if !v.is_finite() || v < 0.0 {
                return Err((400, "invalid_price", "价格必须是非负有限数"));
            }
        }
        Ok(())
    }

    /// 价格条目是否会被某个渠道模型命中(与 find_price 的匹配口径一致):
    /// 末尾 * 按前缀覆盖判断,单独的 * 恒命中,否则精确按名匹配。
    fn price_in_use(entry_model: &str, models: &HashSet<String>) -> bool {
        if let Some(prefix) = entry_model.strip_suffix('*') {
            models.iter().any(|m| m.starts_with(prefix))
        } else {
            models.contains(entry_model)
        }
    }

    /// 规范化过期时间(迁自旧 normalize_expires_at):只收
    /// `YYYY-MM-DD`(补 23:59:59)或 `YYYY-MM-DD HH:MM:SS`(UTC),
    /// 拒绝 RFC3339 'T'(与 SQLite datetime 字符串比较语义错乱);
    /// 空/缺省 → None(永不过期)。形状之外还校验月/日/时/分/秒取值。
    fn normalize_expires_at(
        raw: Option<&str>,
    ) -> Result<Option<String>, (u16, &'static str, &'static str)> {
        let bad = (
            400,
            "invalid_expires_at",
            "expires_at 只支持 YYYY-MM-DD 或 YYYY-MM-DD HH:MM:SS(UTC)",
        );
        let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        // 非法取值(如 99:99:99)会让与 datetime('now') 的字符串比较语义
        // 错乱(恒大于 now 实际永不过期),日上限按 31 粗校验,不逐月精确。
        let num = |r: &str| r.parse::<u32>().unwrap_or(u32::MAX);
        let valid_date = |d: &str| {
            d.len() == 10
                && d.as_bytes()[4] == b'-'
                && d.as_bytes()[7] == b'-'
                && d.bytes()
                    .enumerate()
                    .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit())
                && (1..=12).contains(&num(&d[5..7]))
                && (1..=31).contains(&num(&d[8..10]))
        };
        let valid_time = |t: &str| {
            t.len() == 8
                && t.as_bytes()[2] == b':'
                && t.as_bytes()[5] == b':'
                && t.bytes()
                    .enumerate()
                    .all(|(i, b)| matches!(i, 2 | 5) || b.is_ascii_digit())
                && num(&t[0..2]) <= 23
                && num(&t[3..5]) <= 59
                && num(&t[6..8]) <= 59
        };
        if valid_date(s) {
            return Ok(Some(format!("{} 23:59:59", s)));
        }
        if s.len() == 19 && s.as_bytes()[10] == b' ' && valid_date(&s[..10]) && valid_time(&s[11..])
        {
            return Ok(Some(s.to_string()));
        }
        Err(bad)
    }

    /// 价格行 → 出参(时间转 RFC3339;cached_price 保持"显式 null =
    /// 缓存按输入价"的旧线格式)。
    fn to_api_price(r: &PriceRecord) -> Price {
        Price {
            id: r.id.clone(),
            model: r.model.clone(),
            prompt_price: r.prompt_price,
            completion_price: r.completion_price,
            cached_price: Some(r.cached_price),
            created_at: crate::sqlite_utc(&r.created_at),
            updated_at: crate::sqlite_utc(&r.updated_at),
        }
    }

    /// 流水行 → 出参(微元转元;空 note 省略;kind 词表映射)。
    fn to_api_tx(r: &TxRecord) -> BalanceTransaction {
        BalanceTransaction {
            id: r.id.clone(),
            user_id: r.user_id.clone(),
            username: Some(r.username.clone()),
            amount: Self::micro_to_yuan(r.amount_micro),
            balance_after: Self::micro_to_yuan(r.balance_after_micro),
            kind: match r.kind.as_str() {
                "adjust" => BalanceTransactionKind::Adjust,
                // 库内 CHECK 约束只允许 recharge/adjust,默认支不可达
                _ => BalanceTransactionKind::Recharge,
            },
            note: (!r.note.is_empty()).then(|| r.note.clone()),
            created_at: crate::sqlite_utc(&r.created_at),
        }
    }

    /// 兑换码行 → 出参(掩码重组;永不含明文/哈希)。
    fn to_api_code(r: &RedemptionCodeRecord) -> RedemptionCode {
        RedemptionCode {
            id: r.id.clone(),
            code_masked: format!("AK-****-****-****-{}", r.code_suffix),
            amount: Self::micro_to_yuan(r.amount_micro),
            batch: r.batch.clone(),
            status: match r.status.as_str() {
                "used" => RedemptionCodeStatus::Used,
                "disabled" => RedemptionCodeStatus::Disabled,
                // 库内 CHECK 约束只允许三值,默认支不可达
                _ => RedemptionCodeStatus::Unused,
            },
            used_by: Some(r.used_by_username.clone()),
            used_at: Some(r.used_at.as_deref().map(crate::sqlite_utc)),
            expires_at: Some(r.expires_at.as_deref().map(crate::sqlite_utc)),
            note: (!r.note.is_empty()).then(|| r.note.clone()),
            created_at: crate::sqlite_utc(&r.created_at),
            expired: r.expired,
        }
    }

    // ---------- 9 个固有方法(签名 = trait 方法去 self) ----------

    /// 管理员手工调账(创建一条余额流水)
    ///
    /// `POST /api/users/{id}/balance-transactions`(旧
    /// POST /api/admin/users/{id}/balance;200 → **201**)。
    /// amount 正=充值 kind='recharge',负=扣减 kind='adjust';
    /// UPDATE 余额与 INSERT 流水同事务(端口保证)。
    pub async fn admin_adjust_user_balance(
        &self,
        id: String,
        body: AdjustBalanceRequest,
    ) -> AdminAdjustUserBalanceResponse {
        // amount(元)必须非零有限(JSON 解析保证有限,防御直接调用)……
        if !body.amount.is_finite() || body.amount == 0.0 {
            return AdminAdjustUserBalanceResponse::BadRequest(problem(
                400,
                "invalid_amount",
                "amount 必须是非零有限数",
            ));
        }
        let amount_micro = Self::yuan_to_micro(body.amount);
        // ……且不低于最小记账单位(0.000001 元;更小值四舍五入后变 0)
        if amount_micro == 0 {
            return AdminAdjustUserBalanceResponse::BadRequest(problem(
                400,
                "invalid_amount",
                "amount 过小,低于最小记账单位(0.000001 元)",
            ));
        }
        // 正=充值负=扣减(与旧实现同以微元符号判)
        let kind = if amount_micro > 0 {
            "recharge"
        } else {
            "adjust"
        };
        let note = body.note.unwrap_or_default().trim().to_string();
        match self
            .store
            .adjust_balance(&id, amount_micro, kind, &note)
            .await
        {
            Ok(Some(balance_micro)) => {
                AdminAdjustUserBalanceResponse::Created(AdjustBalanceResult {
                    balance: Self::micro_to_yuan(balance_micro),
                    kind: if amount_micro > 0 {
                        AdjustBalanceResultKind::Recharge
                    } else {
                        AdjustBalanceResultKind::Adjust
                    },
                })
            }
            // 用户不存在:404(不区分"从未存在/已禁用",旧行为)
            Ok(None) => AdminAdjustUserBalanceResponse::NotFound(problem(
                404,
                "not_found",
                "user not found",
            )),
            Err(e) => {
                // 枚举无 5xx 变体(spec 缺口,见文件头映射表):400 + 真实短码
                let code = match &e {
                    BillingStoreError::Unavailable => "internal_error",
                    _ => "update_failed",
                };
                tracing::error!("Failed to adjust balance for user {id}: {e:?}");
                AdminAdjustUserBalanceResponse::BadRequest(problem(400, code, "调账失败"))
            }
        }
    }

    /// 新建价格条目
    ///
    /// `POST /api/prices`(旧 POST /api/admin/prices):201 保留;
    /// 通配校验(400 invalid_model)、409 duplicate_model 保留;
    /// 价格缺省 0、cached_price 创建时 None/显式 null 都存 NULL。
    pub async fn admin_create_price(&self, body: UpsertPriceRequest) -> AdminCreatePriceResponse {
        if let Err((status, code, detail)) = Self::validate_price_fields(&body, true) {
            return AdminCreatePriceResponse::BadRequest(problem(status, code, detail));
        }
        let model = body.model.clone().unwrap_or_default().trim().to_string();
        match self
            .store
            .insert_price(
                &model,
                body.prompt_price.unwrap_or(0.0),
                body.completion_price.unwrap_or(0.0),
                body.cached_price.flatten(),
            )
            .await
        {
            Ok(rec) => {
                bump_price_table_epoch();
                AdminCreatePriceResponse::Created(Self::to_api_price(&rec))
            }
            Err(BillingStoreError::Constraint) => AdminCreatePriceResponse::Conflict(problem(
                409,
                "duplicate_model",
                "该模型(或通配)的价格已存在,请直接编辑",
            )),
            Err(e) => {
                let code = match &e {
                    BillingStoreError::Unavailable => "internal_error",
                    BillingStoreError::Failed(_) => "create_failed",
                    BillingStoreError::Constraint => unreachable!(),
                };
                tracing::error!("Failed to create price for model {model}: {e:?}");
                AdminCreatePriceResponse::BadRequest(problem(400, code, "创建价格失败"))
            }
        }
    }

    /// 删除价格条目
    ///
    /// `DELETE /api/prices/{id}`(旧 200 {message:'deleted'} → **204**)。
    pub async fn admin_delete_price(&self, id: String) -> AdminDeletePriceResponse {
        match self.store.delete_price(&id).await {
            Ok(true) => {
                bump_price_table_epoch();
                AdminDeletePriceResponse::NoContent
            }
            Ok(false) => {
                AdminDeletePriceResponse::NotFound(problem(404, "not_found", "price not found"))
            }
            Err(e) => {
                let code = match &e {
                    BillingStoreError::Unavailable => "internal_error",
                    _ => "delete_failed",
                };
                tracing::error!("Failed to delete price {id}: {e:?}");
                AdminDeletePriceResponse::Forbidden(problem(403, code, "删除价格失败"))
            }
        }
    }

    /// 禁用未使用的兑换码
    ///
    /// `DELETE /api/redemption-codes/{id}`(旧 POST .../disable;
    /// 200 {message} → **204**;409 not_unused / 404 保留)。
    /// 生命周期单调:unused → used/disabled,禁用即终结。
    pub async fn admin_disable_redemption_code(
        &self,
        id: String,
    ) -> AdminDisableRedemptionCodeResponse {
        match self.store.disable_redemption_code(&id).await {
            Ok(DisableOutcome::Disabled) => AdminDisableRedemptionCodeResponse::NoContent,
            Ok(DisableOutcome::NotUnused) => AdminDisableRedemptionCodeResponse::Conflict(problem(
                409,
                "not_unused",
                "只有未使用的兑换码可以禁用",
            )),
            Ok(DisableOutcome::NotFound) => AdminDisableRedemptionCodeResponse::NotFound(problem(
                404,
                "not_found",
                "redemption code not found",
            )),
            Err(e) => {
                let code = match &e {
                    BillingStoreError::Unavailable => "internal_error",
                    _ => "update_failed",
                };
                tracing::error!("Failed to disable redemption code {id}: {e:?}");
                AdminDisableRedemptionCodeResponse::Forbidden(problem(403, code, "禁用失败"))
            }
        }
    }

    /// 调账/充值流水查询
    ///
    /// `GET /api/balance-transactions`(旧 GET
    /// /api/admin/billing/transactions):裸数组 → `{items,total}`;
    /// limit 由固定 200 → 可传(默认 50,clamp 1..=200);
    /// `?user_id=`(空 = 全部)保留;amount/balance_after 出参为元。
    pub async fn admin_list_billing_transactions(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
        user_id: ::std::option::Option<String>,
    ) -> AdminListBillingTransactionsResponse {
        let limit = Self::page_limit(limit);
        let offset = Self::page_offset(offset);
        let uid = user_id.as_deref().map(str::trim).unwrap_or("");
        let filter = (!uid.is_empty()).then_some(uid);
        match self.store.list_transactions(filter, limit, offset).await {
            Ok((total, rows)) => AdminListBillingTransactionsResponse::Ok(BalanceTransactionPage {
                total,
                items: rows.iter().map(Self::to_api_tx).collect(),
            }),
            Err(e) => {
                let code = match &e {
                    BillingStoreError::Unavailable => "internal_error",
                    _ => "query_failed",
                };
                tracing::error!("Failed to list billing transactions: {e:?}");
                AdminListBillingTransactionsResponse::Forbidden(problem(403, code, "查询流水失败"))
            }
        }
    }

    /// 模型价格表(管理端全量,含通配条目)
    ///
    /// `GET /api/prices`(旧 GET /api/admin/prices):裸数组 →
    /// `{items,total}` + limit/offset 分页;`?in_use=1` 只返回会被渠道
    /// 已添加模型命中的条目(**非 '1' 值静默忽略、等同缺省**,api-mapping
    /// 备注 9 的宽容语义,刻意的);cached_price 可 null 保留。
    pub async fn admin_list_prices(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
        in_use: ::std::option::Option<String>,
    ) -> AdminListPricesResponse {
        let limit = Self::page_limit(limit);
        let offset = Self::page_offset(offset);
        let records = match self.store.all_prices().await {
            Ok(r) => r,
            Err(e) => {
                let code = match &e {
                    BillingStoreError::Unavailable => "internal_error",
                    _ => "query_failed",
                };
                tracing::error!("Failed to list prices: {e:?}");
                return AdminListPricesResponse::Forbidden(problem(403, code, "查询价格失败"));
            }
        };
        let mut filtered: Vec<&PriceRecord> = records.iter().collect();
        if in_use.as_deref() == Some("1") {
            let models = self.store.all_channel_models().await;
            filtered.retain(|p| Self::price_in_use(&p.model, &models));
        }
        let total = filtered.len() as i64;
        let items = filtered
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(Self::to_api_price)
            .collect();
        AdminListPricesResponse::Ok(PricePage { items, total })
    }

    /// 兑换码列表(掩码)
    ///
    /// `GET /api/redemption-codes`(旧 GET /api/admin/redemption-codes):
    /// 已是 `{total,items}`,对齐为 `{items,total}`;status 枚举(含计算态
    /// expired)+ 布尔 expired 字段、掩码永不还原、明文/哈希永不出库
    /// 均保留。status 非 0 枚举值现在被生成层拦成 422(旧 400
    /// invalid_status 不可达,spec 描述与生成器行为之差记 notes)。
    pub async fn admin_list_redemption_codes(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
        batch: ::std::option::Option<String>,
        status: ::std::option::Option<AdminListRedemptionCodesStatus>,
    ) -> AdminListRedemptionCodesResponse {
        let limit = Self::page_limit(limit);
        let offset = Self::page_offset(offset);
        let batch = batch.as_deref().map(str::trim).unwrap_or("");
        let batch_filter = (!batch.is_empty()).then_some(batch);
        let status_filter = status.map(|s| match s {
            AdminListRedemptionCodesStatus::Unused => "unused",
            AdminListRedemptionCodesStatus::Used => "used",
            AdminListRedemptionCodesStatus::Disabled => "disabled",
            AdminListRedemptionCodesStatus::Expired => "expired",
        });
        match self
            .store
            .list_redemption_codes(batch_filter, status_filter, limit, offset)
            .await
        {
            Ok((total, rows)) => AdminListRedemptionCodesResponse::Ok(RedemptionCodePage {
                total,
                items: rows.iter().map(Self::to_api_code).collect(),
            }),
            Err(e) => {
                let code = match &e {
                    BillingStoreError::Unavailable => "internal_error",
                    _ => "query_failed",
                };
                tracing::error!("Failed to list redemption codes: {e:?}");
                AdminListRedemptionCodesResponse::Forbidden(problem(403, code, "查询兑换码失败"))
            }
        }
    }

    /// 批量生成兑换码
    ///
    /// `POST /api/redemption-codes`(旧 POST /api/admin/redemption-codes):
    /// 201 + **明文仅一次**;expires_at 只收 `YYYY-MM-DD[ HH:MM:SS]`
    /// (拒 RFC3339 'T');整批一个事务,碰撞 500 code_collision。
    /// count 1..=500 现由生成层 schema 拦(422),impl 侧校验防御直接
    /// 调用方(见文件头"装配契约")。
    pub async fn admin_create_redemption_codes(
        &self,
        body: CreateRedemptionCodesRequest,
    ) -> AdminCreateRedemptionCodesResponse {
        // 防御性:HTTP 路径上 schema(minimum:1/maximum:500)已拦成 422
        if body.count <= 0 || body.count > 500 {
            return AdminCreateRedemptionCodesResponse::BadRequest(problem(
                400,
                "invalid_count",
                "count 须在 1..=500 之间",
            ));
        }
        if !body.amount.is_finite() || body.amount <= 0.0 {
            return AdminCreateRedemptionCodesResponse::BadRequest(problem(
                400,
                "invalid_amount",
                "amount 必须是正数(元)",
            ));
        }
        // 面值上限:浮点饱和转换会把 1e20 这类输入静默变成约 92 万亿元面值
        if body.amount > 1_000_000.0 {
            return AdminCreateRedemptionCodesResponse::BadRequest(problem(
                400,
                "invalid_amount",
                "amount 不能超过 1,000,000 元",
            ));
        }
        let amount_micro = Self::yuan_to_micro(body.amount);
        if amount_micro <= 0 {
            return AdminCreateRedemptionCodesResponse::BadRequest(problem(
                400,
                "invalid_amount",
                "amount 低于最小记账单位(0.000001 元)",
            ));
        }
        let batch = body.batch.trim().to_string();
        // schema maxLength:64 拦原始串;trim 后为空的批次仍要拒(空批次无意义)
        if batch.is_empty() || batch.chars().count() > 64 {
            return AdminCreateRedemptionCodesResponse::BadRequest(problem(
                400,
                "invalid_batch",
                "batch 不能为空且不超过 64 字符",
            ));
        }
        let expires_at = match Self::normalize_expires_at(body.expires_at.as_deref()) {
            Ok(v) => v,
            Err((status, code, detail)) => {
                return AdminCreateRedemptionCodesResponse::BadRequest(problem(
                    status, code, detail,
                ));
            }
        };
        let note = body.note.clone().unwrap_or_default().trim().to_string();

        // 明文只在内存中组装进响应;库中只落 sha256 与掩码后缀。
        let codes: Vec<String> = (0..body.count)
            .map(|_| self.store.generate_code())
            .collect();
        let rows: Vec<NewRedemptionCode> = codes
            .iter()
            .map(|code| NewRedemptionCode {
                code_hash: self.store.hash_code(code),
                // 掩码展示用后缀(明文末 4 位;字符集 ASCII,切尾安全)
                code_suffix: code[code.len() - 4..].to_string(),
                amount_micro,
                batch: batch.clone(),
                expires_at: expires_at.clone(),
                note: note.clone(),
            })
            .collect();
        match self.store.insert_redemption_codes(&rows).await {
            Ok(()) => AdminCreateRedemptionCodesResponse::Created(CreatedRedemptionCodes {
                batch,
                amount: Self::micro_to_yuan(amount_micro),
                count: codes.len() as i64,
                codes,
            }),
            // 2^80 空间下碰撞几乎不可能;真撞上重试一次生成即可,直接报错
            // 让管理员重新提交,不做静默丢弃(哈希已落库而明文未送达的码
            // 永远无法兑换,整批回滚后重来是唯一安全路径)。
            Err(BillingStoreError::Constraint) => {
                tracing::error!("Redemption code collision on insert (retry the request)");
                AdminCreateRedemptionCodesResponse::InternalServerError(problem(
                    500,
                    "code_collision",
                    "码生成碰撞,请重试",
                ))
            }
            Err(e) => {
                let code = match &e {
                    BillingStoreError::Unavailable => "internal_error",
                    BillingStoreError::Failed(_) => "create_failed",
                    BillingStoreError::Constraint => unreachable!(),
                };
                tracing::error!("Failed to create redemption codes: {e:?}");
                AdminCreateRedemptionCodesResponse::InternalServerError(problem(
                    500,
                    code,
                    "生成兑换码失败",
                ))
            }
        }
    }

    /// 部分更新价格条目
    ///
    /// `PATCH /api/prices/{id}`(旧同路径;成功已回资源,保留):
    /// **cached_price 三态**——缺省不改动 / 显式 null 清回 NULL(缓存按
    /// 输入价)/ 数值更新;改 model 撞唯一约束 409;无字段 400
    /// no_fields_to_update。
    pub async fn admin_update_price(
        &self,
        id: String,
        body: UpsertPriceRequest,
    ) -> AdminUpdatePriceResponse {
        if let Err((status, code, detail)) = Self::validate_price_fields(&body, false) {
            return AdminUpdatePriceResponse::BadRequest(problem(status, code, detail));
        }
        // 空 PATCH(所有字段缺省)在 impl 拦——schema 对 UpsertPriceRequest
        // 刻意不设 minProperties(创建时空 body 也不合法但创建有 model
        // 必填的 impl 校验兜着;空对象 → invalid_model)。
        let model = body
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        if body.model.is_none()
            && body.prompt_price.is_none()
            && body.completion_price.is_none()
            && body.cached_price.is_none()
        {
            return AdminUpdatePriceResponse::BadRequest(problem(
                400,
                "no_fields_to_update",
                "no fields to update",
            ));
        }
        match self
            .store
            .update_price(
                &id,
                model.as_deref(),
                body.prompt_price,
                body.completion_price,
                body.cached_price,
            )
            .await
        {
            Ok(Some(rec)) => {
                bump_price_table_epoch();
                AdminUpdatePriceResponse::Ok(Self::to_api_price(&rec))
            }
            Ok(None) => {
                AdminUpdatePriceResponse::NotFound(problem(404, "not_found", "price not found"))
            }
            Err(BillingStoreError::Constraint) => AdminUpdatePriceResponse::Conflict(problem(
                409,
                "duplicate_model",
                "该模型(或通配)的价格已存在",
            )),
            Err(e) => {
                let code = match &e {
                    BillingStoreError::Unavailable => "internal_error",
                    BillingStoreError::Failed(_) => "update_failed",
                    BillingStoreError::Constraint => unreachable!(),
                };
                tracing::error!("Failed to update price {id}: {e:?}");
                AdminUpdatePriceResponse::BadRequest(problem(400, code, "更新价格失败"))
            }
        }
    }
}
