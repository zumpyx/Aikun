//! 协议契约测试(refactor-plan 9.1):黄金样本比对完整输出,而非个别字段。
//!
//! 样本分三层(见 9.1 参照体系):
//! - 输入(request / upstream_response):手写,覆盖 tool_calls 与 usage(含缓存拆分);
//! - 期望输出(golden):由当前实现生成——`UPDATE_FIXTURES=1 cargo test -p aikun-protocol`
//!   重写;基线即契约,转换行为有意变更时必须显式更新 golden 并记 CHANGELOG;
//! - reference/:OpenAI 官方 spec 快照(MIT),形状下界参照,不参与断言。
//!
//! 挥发性字段(top-level id / created 由实现注入 uuid/时间戳)比对前统一摘除。
//!
//! 流式样本(9.1 阶段 2):`{dir}/stream/<case>/` 两套,`input.jsonl` 为上游
//! SSE 事件序列(手写,每行一个 {event, data} 对应 SseEvent;data 为对象即
//! 事件 JSON,为字符串即 [DONE] 这类裸 data 帧),`expected.jsonl` 为客户端侧
//! 事件序列(golden,UPDATE_FIXTURES=1 重建)。逐事件喂 StreamConverter::push、
//! 收尾 finish(),event 名 + data 逐字段完整比对;比对前沿用 mask_volatile
//! 摘 chunk 顶层的 id/created(A→O 每个 chunk 都带实现注入的 created 时间戳),
//! 另摘 Anthropic message_start 嵌套的 message.id(上游首帧缺 id 时为生成的
//! uuid)。

use aikun_protocol::{
    Protocol, SseEvent, SseOut, StreamConverter, convert_request, convert_response,
};
use serde_json::{Value, json};

fn load(dir: &str, name: &str) -> Value {
    let path = format!("tests/fixtures/{dir}/{name}");
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
        .unwrap()
}

/// 摘除 top-level 的 id / created(实现注入 uuid / 当前时间,不可作黄金断言)。
fn mask_volatile(mut v: Value) -> Value {
    if let Value::Object(map) = &mut v {
        map.remove("id");
        map.remove("created");
    }
    v
}

fn check_pair(dir: &str, from: Protocol, to: Protocol, client_model: &str) {
    let request = load(dir, "request.json");
    let upstream = load(dir, "upstream_response.json");

    let converted_request = convert_request(&request, from, to);
    let converted_response = convert_response(&upstream, from, to, client_model);

    let update = std::env::var("UPDATE_FIXTURES").as_deref() == Ok("1");
    for (name, value) in [
        ("expected_request.json", converted_request),
        ("expected_response.json", converted_response),
    ] {
        let path = format!("tests/fixtures/{dir}/{name}");
        if update {
            std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap() + "\n").unwrap();
            continue;
        }
        let golden = load(dir, name);
        assert_eq!(
            mask_volatile(golden),
            mask_volatile(value),
            "{dir}/{name} 与转换输出不一致(有意变更须 UPDATE_FIXTURES=1 重建并记 CHANGELOG)"
        );
    }
}

#[test]
fn openai_to_anthropic_nonstream() {
    check_pair(
        "openai-to-anthropic",
        Protocol::OpenAi,
        Protocol::Anthropic,
        "gpt-4o",
    );
}

#[test]
fn anthropic_to_openai_nonstream() {
    check_pair(
        "anthropic-to-openai",
        Protocol::Anthropic,
        Protocol::OpenAi,
        "claude-3-5-sonnet",
    );
}

// ----------------------------------------------------------------------------
// 流式黄金样本(9.1 阶段 2)
// ----------------------------------------------------------------------------

/// 读 JSONL 事件序列;跳过空行与 `//` 注释行(fixture 文件头注明构造依据)。
fn load_jsonl(path: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    text.lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("//")
        })
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("{path}: 行不是合法 JSON({e}): {l}"))
        })
        .collect()
}

/// JSONL 行 {event, data} → SseEvent。data 为字符串时是裸 data 帧(如
/// [DONE]),原样取用;为对象/其他 JSON 值时序列化为紧凑 JSON 字符串。
fn sse_event_from(line: &Value) -> SseEvent {
    let data = match &line["data"] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    SseEvent {
        event: line["event"].as_str().map(str::to_string),
        data,
    }
}

/// SseOut → JSONL 行 {event, data}。data 能解析为 JSON 就按结构比对
/// (对象等值比较即逐字段比对),否则([DONE] 等裸帧)退回原字符串。
fn sse_out_to_line(out: &SseOut) -> Value {
    let data = serde_json::from_str::<Value>(&out.data)
        .unwrap_or_else(|_| Value::String(out.data.clone()));
    json!({ "event": out.event, "data": data })
}

/// 流式事件比对前摘除挥发性字段:对 data 复用 mask_volatile(顶层 id/created),
/// 另对嵌套的 message.message_start 场景摘 message.id(上游首帧缺 id 时
/// 实现会注入 uuid)。event 名保留参与比对。
fn mask_stream_event(mut line: Value) -> Value {
    if let Some(data) = line.get_mut("data") {
        *data = mask_volatile(data.take());
        if let Some(msg) = data.get_mut("message") {
            *msg = mask_volatile(msg.take());
        }
    }
    line
}

/// 逐事件喂 StreamConverter::push,末尾 finish(),与 expected.jsonl 完整比对。
/// `client` / `provider` 是客户端与上游线协议:目录名与 check_pair 一致表示
/// 转换方向(from→to),故 openai-to-anthropic 流式输入是 OpenAI chunk、
/// 输出是 Anthropic 事件(client=Anthropic, provider=OpenAi)。
fn check_stream_case(
    dir: &str,
    client: Protocol,
    provider: Protocol,
    model: &str,
    case: &str,
    note: &str,
) {
    let base = format!("tests/fixtures/{dir}/{case}");
    let input = load_jsonl(&format!("{base}/input.jsonl"));

    let mut converter = StreamConverter::new(client, provider, model);
    let mut produced = Vec::new();
    for line in &input {
        produced.extend(
            converter
                .push(&sse_event_from(line))
                .iter()
                .map(sse_out_to_line),
        );
    }
    produced.extend(converter.finish().iter().map(sse_out_to_line));

    let expected_path = format!("{base}/expected.jsonl");
    if std::env::var("UPDATE_FIXTURES").as_deref() == Ok("1") {
        let mut file = format!("// {note}\n");
        for line in &produced {
            file += &serde_json::to_string(line).unwrap();
            file.push('\n');
        }
        std::fs::write(&expected_path, file).unwrap();
        return;
    }
    let golden: Vec<Value> = load_jsonl(&expected_path)
        .into_iter()
        .map(mask_stream_event)
        .collect();
    let actual: Vec<Value> = produced.into_iter().map(mask_stream_event).collect();
    assert_eq!(
        golden, actual,
        "{dir}/{case}/expected.jsonl 与流式转换输出不一致(有意变更须 UPDATE_FIXTURES=1 重建并记 CHANGELOG)"
    );
}

#[test]
fn openai_to_anthropic_stream() {
    let dir = "openai-to-anthropic/stream";
    for (case, note) in [
        (
            "text-with-usage",
            "黄金样本(由实现生成,勿手改):文本增量 + usage 缓存拆分 + stop→end_turn + [DONE] 收尾",
        ),
        (
            "tool-calls-fragmented",
            "黄金样本(由实现生成,勿手改):tool_calls 分片拼装 input_json_delta + 块开合顺序 + tool_calls→tool_use",
        ),
        (
            "eof-without-done",
            "黄金样本(由实现生成,勿手改):上游 EOF 无 [DONE],finish() 补齐收尾 + length→max_tokens + usage 零回填",
        ),
        (
            "error-frame-halts",
            "黄金样本(由实现生成,勿手改):流内 error 帧转 Anthropic error 事件并停机,其后不再产出",
        ),
    ] {
        check_stream_case(
            dir,
            Protocol::Anthropic,
            Protocol::OpenAi,
            "gpt-4o",
            case,
            note,
        );
    }
}

#[test]
fn anthropic_to_openai_stream() {
    let dir = "anthropic-to-openai/stream";
    for (case, note) in [
        (
            "text-with-usage",
            "黄金样本(由实现生成,勿手改):message_start 缓存拆分 + 文本增量 + end_turn→stop + usage 回填 + [DONE]",
        ),
        (
            "tool-calls-fragmented",
            "黄金样本(由实现生成,勿手改):tool_use 块与 input_json_delta 分片 → tool_calls 增量 + tool_use→tool_calls",
        ),
        (
            "eof-without-terminal",
            "黄金样本(由实现生成,勿手改):thinking→reasoning_content,signature/ping 丢弃,EOF 无终态事件仅补 [DONE]",
        ),
    ] {
        check_stream_case(
            dir,
            Protocol::OpenAi,
            Protocol::Anthropic,
            "claude-3-5-sonnet",
            case,
            note,
        );
    }
}
