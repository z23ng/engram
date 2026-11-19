use mem_mcp::handle_jsonrpc_request;
use mem_search::{ConsolidateLlmConfig, MemSearchEngine};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};
use serde_json::{json, Value};

fn call(engine: &MemSearchEngine, req: Value) -> Value {
    let llm = ConsolidateLlmConfig::disabled();
    handle_jsonrpc_request(engine.vcs(), engine, &llm, &req).expect("response")
}

#[test]
fn library_dispatches_tools_without_stdio() {
    let vcs = MemVcs::new(SqliteStore::open_in_memory().unwrap());
    vcs.init().unwrap();
    let engine = MemSearchEngine::new_bm25_only(vcs);

    let list = call(
        &engine,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    );
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 11);

    let write = call(
        &engine,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"mem_write","arguments":{"path":"/semantic/a.md","content":"alpha"}}}),
    );
    assert_eq!(write["result"]["isError"], false);

    let read = call(
        &engine,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"mem_read","arguments":{"path":"/semantic/a.md"}}}),
    );
    assert!(read["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("alpha"));

    let notify = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
    let llm = ConsolidateLlmConfig::disabled();
    assert!(handle_jsonrpc_request(engine.vcs(), &engine, &llm, &notify).is_none());
}
