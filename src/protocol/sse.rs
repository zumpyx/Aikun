//! SSE 字节流解析:事件边界(LF / CRLF / CR)、多行 data、event 字段。



#[allow(unused_imports)]
use crate::protocol::*;

/// One parsed SSE event (possibly multiple data: lines joined).
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Drain all complete SSE events (terminated by a blank line) from `buf`.
pub fn parse_sse_events(buf: &mut Vec<u8>) -> Vec<SseEvent> {
    let mut events = Vec::new();
    while let Some((idx, len)) = find_event_boundary(buf) {
        let raw: Vec<u8> = buf.drain(..idx + len).collect();
        if let Some(ev) = parse_sse_block(&raw[..idx]) {
            events.push(ev);
        }
    }
    events
}

/// Parse whatever remains in the buffer as a final event (stream ended
/// without a trailing blank line).
pub fn parse_sse_remaining(buf: &mut Vec<u8>) -> Option<SseEvent> {
    if buf.is_empty() {
        return None;
    }
    let raw = std::mem::take(buf);
    parse_sse_block(&raw)
}

pub(crate) fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        // 少数上游用裸 \r 分隔事件
        if buf[i] == b'\r' && buf[i + 1] == b'\r' {
            return Some((i, 2));
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

pub(crate) fn parse_sse_block(raw: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(raw);
    let mut event = None;
    let mut data_lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data:") {
            data_lines.push(v.trim_start_matches(' ').to_string());
        }
    }
    if data_lines.is_empty() {
        None
    } else {
        Some(SseEvent {
            event,
            data: data_lines.join("\n"),
        })
    }
}


#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::protocol::test_util::{out_types, sse, sse_typed};
    // ---- SSE parsing ----

    #[test]
    fn sse_lf_boundary() {
        let mut buf = b"data: one\n\ndata: two\n\npartial".to_vec();
        let events = parse_sse_events(&mut buf);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "two");
        assert_eq!(buf, b"partial");
    }

    #[test]
    fn sse_crlf_boundary() {
        let mut buf = b"data: one\r\n\r\ndata: two\r\n\r\n".to_vec();
        let events = parse_sse_events(&mut buf);
        assert_eq!(events.len(), 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn sse_bare_cr_boundary() {
        let mut buf = b"data: one\r\rdata: two\r\rpartial".to_vec();
        let events = parse_sse_events(&mut buf);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "two");
        assert_eq!(buf, b"partial");
    }

    #[test]
    fn sse_split_across_chunks() {
        let mut buf = b"data: hel".to_vec();
        assert!(parse_sse_events(&mut buf).is_empty());
        buf.extend_from_slice(b"lo\n\n");
        let events = parse_sse_events(&mut buf);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn sse_multiline_data_and_event_field() {
        let mut buf = b"event: message_start\ndata: first line\ndata: second line\n\n".to_vec();
        let events = parse_sse_events(&mut buf);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "first line\nsecond line");
    }

    #[test]
    fn sse_block_without_data_ignored() {
        let mut buf = b": ping comment\n\nevent: only-event\n\n".to_vec();
        assert!(parse_sse_events(&mut buf).is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn sse_remaining_parses_tail() {
        let mut buf = b"data: tail".to_vec();
        let ev = parse_sse_remaining(&mut buf).unwrap();
        assert_eq!(ev.data, "tail");
        assert!(buf.is_empty());
        assert!(parse_sse_remaining(&mut buf).is_none());
    }
}
