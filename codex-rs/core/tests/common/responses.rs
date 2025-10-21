use std::sync::Arc;
use std::sync::Mutex;

use serde_json::Value;
use wiremock::BodyPrintLimit;
use wiremock::Match;
use wiremock::Mock;
use wiremock::MockBuilder;
use wiremock::MockServer;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

#[derive(Debug, Clone)]
pub struct ResponseMock {
    requests: Arc<Mutex<Vec<ResponsesRequest>>>,
}

impl ResponseMock {
    fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn single_request(&self) -> ResponsesRequest {
        let requests = self.requests.lock().unwrap();
        if requests.len() != 1 {
            panic!("expected 1 request, got {}", requests.len());
        }
        requests.first().unwrap().clone()
    }

    pub fn requests(&self) -> Vec<ResponsesRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone)]
pub struct ResponsesRequest(wiremock::Request);

impl ResponsesRequest {
    pub fn body_json(&self) -> Value {
        self.0.body_json().unwrap()
    }

    pub fn input(&self) -> Vec<Value> {
        self.0.body_json::<Value>().unwrap()["input"]
            .as_array()
            .expect("input array not found in request")
            .clone()
    }

    pub fn function_call_output(&self, call_id: &str) -> Value {
        self.call_output(call_id, "function_call_output")
    }

    pub fn custom_tool_call_output(&self, call_id: &str) -> Value {
        self.call_output(call_id, "custom_tool_call_output")
    }

    pub fn call_output(&self, call_id: &str, call_type: &str) -> Value {
        self.input()
            .iter()
            .find(|item| {
                item.get("type").unwrap() == call_type && item.get("call_id").unwrap() == call_id
            })
            .cloned()
            .unwrap_or_else(|| panic!("function call output {call_id} item not found in request"))
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.0
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    pub fn path(&self) -> String {
        self.0.url.path().to_string()
    }

    pub fn query_param(&self, name: &str) -> Option<String> {
        self.0
            .url
            .query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.to_string())
    }
}

impl Match for ResponseMock {
    fn matches(&self, request: &wiremock::Request) -> bool {
        self.requests
            .lock()
            .unwrap()
            .push(ResponsesRequest(request.clone()));
        true
    }
}

/// Build an SSE stream body from a list of JSON events.
pub fn sse(events: Vec<Value>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for ev in events {
        let kind = ev.get("type").and_then(|v| v.as_str()).unwrap();
        writeln!(&mut out, "event: {kind}").unwrap();
        if !ev.as_object().map(|o| o.len() == 1).unwrap_or(false) {
            write!(&mut out, "data: {ev}\n\n").unwrap();
        } else {
            out.push('\n');
        }
    }
    out
}

/// Convenience: SSE event for a completed response with a specific id.
pub fn ev_completed(id: &str) -> Value {
    serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {"input_tokens":0,"input_tokens_details":null,"output_tokens":0,"output_tokens_details":null,"total_tokens":0}
        }
    })
}

/// Convenience: SSE event for a created response with a specific id.
pub fn ev_response_created(id: &str) -> Value {
    serde_json::json!({
        "type": "response.created",
        "response": {
            "id": id,
        }
    })
}

pub fn ev_completed_with_tokens(id: &str, total_tokens: i64) -> Value {
    serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {
                "input_tokens": total_tokens,
                "input_tokens_details": null,
                "output_tokens": 0,
                "output_tokens_details": null,
                "total_tokens": total_tokens
            }
        }
    })
}

/// Convenience: SSE event for a single assistant message output item.
pub fn ev_assistant_message(id: &str, text: &str) -> Value {
    serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "id": id,
            "content": [{"type": "output_text", "text": text}]
        }
    })
}

pub fn ev_function_call(call_id: &str, name: &str, arguments: &str) -> Value {
    serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": arguments
        }
    })
}

pub fn ev_custom_tool_call(call_id: &str, name: &str, input: &str) -> Value {
    serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "custom_tool_call",
            "call_id": call_id,
            "name": name,
            "input": input
        }
    })
}

pub fn ev_local_shell_call(call_id: &str, status: &str, command: Vec<&str>) -> Value {
    serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "local_shell_call",
            "call_id": call_id,
            "status": status,
            "action": {
                "type": "exec",
                "command": command,
            }
        }
    })
}

/// Convenience: SSE event for an `apply_patch` custom tool call with raw patch
/// text. This mirrors the payload produced by the Responses API when the model
/// invokes `apply_patch` directly (before we convert it to a function call).
pub fn ev_apply_patch_custom_tool_call(call_id: &str, patch: &str) -> Value {
    serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "custom_tool_call",
            "name": "apply_patch",
            "input": patch,
            "call_id": call_id
        }
    })
}

/// Convenience: SSE event for an `apply_patch` function call. The Responses API
/// wraps the patch content in a JSON string under the `input` key; we recreate
/// the same structure so downstream code exercises the full parsing path.
pub fn ev_apply_patch_function_call(call_id: &str, patch: &str) -> Value {
    let arguments = serde_json::json!({ "input": patch });
    let arguments = serde_json::to_string(&arguments).expect("serialize apply_patch arguments");

    serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "name": "apply_patch",
            "arguments": arguments,
            "call_id": call_id
        }
    })
}

pub fn sse_failed(id: &str, code: &str, message: &str) -> String {
    sse(vec![serde_json::json!({
        "type": "response.failed",
        "response": {
            "id": id,
            "error": {"code": code, "message": message}
        }
    })])
}

pub fn sse_response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(body, "text/event-stream")
}

fn base_mock() -> (MockBuilder, ResponseMock) {
    let response_mock = ResponseMock::new();
    let mock = Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(response_mock.clone());
    (mock, response_mock)
}

pub async fn mount_sse_once_match<M>(server: &MockServer, matcher: M, body: String) -> ResponseMock
where
    M: wiremock::Match + Send + Sync + 'static,
{
    let (mock, response_mock) = base_mock();
    mock.and(matcher)
        .respond_with(sse_response(body))
        .up_to_n_times(1)
        .mount(server)
        .await;
    response_mock
}

pub async fn mount_sse_once(server: &MockServer, body: String) -> ResponseMock {
    let (mock, response_mock) = base_mock();
    mock.respond_with(sse_response(body))
        .up_to_n_times(1)
        .mount(server)
        .await;
    response_mock
}

pub async fn mount_sse(server: &MockServer, body: String) -> ResponseMock {
    let (mock, response_mock) = base_mock();
    mock.respond_with(sse_response(body)).mount(server).await;
    response_mock
}

pub async fn start_mock_server() -> MockServer {
    MockServer::builder()
        .body_print_limit(BodyPrintLimit::Limited(80_000))
        .start()
        .await
}

/// Mounts a sequence of SSE response bodies and serves them in order for each
/// POST to `/v1/responses`. Panics if more requests are received than bodies
/// provided. Also asserts the exact number of expected calls.
pub async fn mount_sse_sequence(server: &MockServer, bodies: Vec<String>) -> ResponseMock {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    struct SeqResponder {
        num_calls: AtomicUsize,
        responses: Vec<String>,
    }

    impl Respond for SeqResponder {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            let call_num = self.num_calls.fetch_add(1, Ordering::SeqCst);
            match self.responses.get(call_num) {
                Some(body) => ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body.clone()),
                None => panic!("no response for {call_num}"),
            }
        }
    }

    let num_calls = bodies.len();
    let responder = SeqResponder {
        num_calls: AtomicUsize::new(0),
        responses: bodies,
    };

    let (mock, response_mock) = base_mock();
    mock.respond_with(responder)
        .expect(num_calls as u64)
        .mount(server)
        .await;

    response_mock
}
