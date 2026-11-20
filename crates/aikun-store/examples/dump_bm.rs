//! 一次性调试工具:打开嵌入式库,解密打印指定渠道的 zcode 业务 token。
//! 用法:AIKUN_DATABASE_URL=surrealkv://./data cargo run -p aikun-store --example dump_bm -- <provider_id>
//! (服务须停——嵌入式 SurrealKV 目录独占锁)
fn main() {
    let url = std::env::var("AIKUN_DATABASE_URL").unwrap_or_else(|_| "surrealkv://./data".into());
    let pid = std::env::args().nth(1).expect("provider id 参数");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_stack_size(16 * 1024 * 1024)
        .build()
        .unwrap();
    rt.block_on(async move {
        let ep = aikun_store::surreal::Endpoint::parse(&url).unwrap();
        let h = aikun_store::surreal::SurrealHandle::connect(&ep)
            .await
            .unwrap();
        let cipher = aikun_store::crypto::KeyCipher::from_secret(
            &std::env::var("AIKUN_ENCRYPTION_KEY").expect("AIKUN_ENCRYPTION_KEY"),
        );
        let row = h
            .db()
            .query("SELECT business_token FROM zcode_oauth WHERE provider_id = $p")
            .bind(("p", pid.clone()))
            .await
            .unwrap();
        let mut resp = row;
        let rows: Vec<serde_json::Value> = resp.take(0).unwrap();
        for r in rows {
            let enc = r["business_token"].as_str().unwrap_or_default();
            if enc.is_empty() {
                continue;
            }
            match cipher.decrypt(enc) {
                Some(plain) => println!("BM={plain}"),
                None => println!("DECRYPT_FAIL len={}", enc.len()),
            }
        }
    });
}
