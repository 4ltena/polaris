//! Home browsing wire: home-relative paths only and responses bound to the request.
use polaris_desktop_protocol::{codec, request::Request, response::Response};
use serde_json::{Value, json};

fn decode<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, codec::CodecError> {
    codec::from_json(&serde_json::to_vec(value).unwrap())
}

fn request(method: &str, params: Value) -> Value {
    json!({"protocol_version":1,"kind":"request","client_id":"client","request_id":"request",
        "method":method,"params":params})
}

fn list_page(directory: &str, entries: Value) -> Value {
    json!({"protocol_version":1,"kind":"response","client_id":"client","request_id":"request",
        "result":{"type":"browse.list","payload":{"browse_epoch":"browse-1","snapshot_id":"s1",
        "directory":directory,"entries":entries,"omitted":"0","capacity":false}}})
}

fn entry(name: &str, path: &str) -> Value {
    json!({"name":name,"path":path,"kind":"file","device":"1","inode":"2"})
}

#[test]
fn only_session_free_home_relative_paths_decode() {
    assert!(decode::<Request>(&request("browse.list", json!({"path":""}))).is_ok());
    assert!(
        decode::<Request>(&request("browse.list", json!({"path":"a/b","cursor":"c1"}))).is_ok()
    );
    let read = json!({"path":"a.txt","device":"1","inode":"2"});
    assert!(decode::<Request>(&request("browse.read", read.clone())).is_ok());
    for bad in [
        "/etc",
        "a//b",
        "./a",
        "a/..",
        "a\u{0}",
        "a\nb",
        &"a".repeat(4097),
    ] {
        assert!(
            decode::<Request>(&request("browse.list", json!({"path":bad}))).is_err(),
            "{bad:?}"
        );
    }
    assert!(
        decode::<Request>(&request(
            "browse.read",
            json!({"path":"","device":"1","inode":"2"})
        ))
        .is_err()
    );
    assert!(decode::<Request>(&request("browse.list", json!({"path":"","cursor":null}))).is_err());
    let mut scoped = request("browse.read", read);
    scoped["session_id"] = json!("session");
    assert!(decode::<Request>(&scoped).is_err());
}

#[test]
fn pages_and_reads_must_answer_the_requested_path() {
    let list: Request = decode(&request("browse.list", json!({"path":"docs"}))).unwrap();
    let good: Response = decode(&list_page("docs", json!([entry("a.txt", "docs/a.txt")]))).unwrap();
    assert!(good.validate_for(&list).is_ok());
    for bad in [
        list_page("other", json!([])),
        list_page("docs", json!([entry("a.txt", "other/a.txt")])),
        list_page("docs", json!([entry("x/a.txt", "docs/x/a.txt")])),
    ] {
        let response: Response = decode(&bad).unwrap();
        assert!(response.validate_for(&list).is_err());
    }
    let read: Request = decode(&request(
        "browse.read",
        json!({"path":"a.txt","device":"1","inode":"2"}),
    ))
    .unwrap();
    let result = |inode: &str, content: Value| {
        json!({"protocol_version":1,"kind":"response","client_id":"client","request_id":"request",
            "result":{"type":"browse.read","payload":{"browse_epoch":"browse-1","path":"a.txt",
            "device":"1","inode":inode,"content":content}}})
    };
    let ready: Response = decode(&result("2", json!({"state":"ready","text":"hello"}))).unwrap();
    assert!(ready.validate_for(&read).is_ok());
    let moved: Response = decode(&result(
        "3",
        json!({"state":"unavailable","reason":"replaced"}),
    ))
    .unwrap();
    assert!(moved.validate_for(&read).is_err());
    for content in [
        json!({"state":"ready"}),
        json!({"state":"unavailable","reason":"replaced","text":""}),
        json!({"state":"ready","text":"a\u{0}"}),
        json!({"state":"unavailable","reason":"os error 2"}),
    ] {
        assert!(decode::<Response>(&result("2", content)).is_err());
    }
}
