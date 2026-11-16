//! Admin tag · users 方法组(用户 CRUD / 批量创建)实现桩。
//!
//! **指派:并行工人·Users**。当前是 todo!() 桩 —— Auth 试点批只预置
//! 轨道;Users 批填充方法体。本文件只写**固有方法**(签名 = trait 方法
//! 去掉 self 的参数,返回同名 Response 枚举);`impl AdminApi for
//! AdminService` 的委托在 lib.rs,由 `AdminService.users` 字段路由到
//! 这里,工人不要改 lib.rs。装配与删旧由集成工人统一做;纪律与写法见
//! docs/migration-playbook.md。**不要改本 crate 的 lib.rs 与其他 impl 文件。**
//!
//! 前置拍板项(plan 步骤 6.4):ID 已定为不透明 string(api-mapping 备注 2,
//! 2026-11-16),本组可直接开工。
// 桩文件:todo!() 体不消费参数;工人填充时删除本 allow。
#![allow(unused_variables, unused_imports)]

use aikun_openapi::openapi::server::errors::*;
use aikun_openapi::openapi::types::*;
/// users 方法组服务。字段与构造函数由 Users 批工人按需定义(过渡期依赖
/// 经端口 trait 注入,参照 auth.rs 的 AuthStore 模式;适配器写在 main.rs,
/// 由集成工人装配)。
#[derive(Clone, Default)]
pub struct AdminUsersService;

impl AdminUsersService {
    /// 创建用户
    ///
    /// `POST /api/users`
    pub async fn admin_create_user(&self, body: CreateUserRequest) -> AdminCreateUserResponse {
        todo!("Users 批:POST /api/users(旧 POST /api/admin/users;409 username_exists 保留)")
    }

    /// 批量创建用户(逐项独立成功/失败)
    ///
    /// `POST /api/users/batch`
    pub async fn admin_create_users_batch(
        &self,
        body: BatchCreateUsersRequest,
    ) -> AdminCreateUsersBatchResponse {
        todo!("Users 批:POST /api/users/batch(整体 200;失败项 error → code)")
    }

    /// 禁用用户(软删除)
    ///
    /// `DELETE /api/users/{id}`
    pub async fn admin_delete_user(&self, id: String) -> AdminDeleteUserResponse {
        todo!("Users 批:DELETE /api/users/{{id}}(软禁用保留;200 {{message}} → 204;last_admin 409)")
    }

    /// 查单个用户(含余额)
    ///
    /// `GET /api/users/{id}`
    pub async fn admin_get_user(&self, id: String) -> AdminGetUserResponse {
        todo!("Users 批:GET /api/users/{{id}}(旧 GET /api/admin/users/{{id}};404 保留)")
    }

    /// 用户列表
    ///
    /// `GET /api/users`
    pub async fn admin_list_users(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
    ) -> AdminListUsersResponse {
        todo!("Users 批:GET /api/users(全量 → {{items,total}} + 分页)")
    }

    /// 部分更新用户(改密/启停/改角色)
    ///
    /// `PATCH /api/users/{id}`
    pub async fn admin_update_user(
        &self,
        id: String,
        body: UpdateUserRequest,
    ) -> AdminUpdateUserResponse {
        todo!(
            "Users 批:PATCH /api/users/{{id}}(返回资源;last_admin 400 → 409;改密 token_version+1)"
        )
    }
}
