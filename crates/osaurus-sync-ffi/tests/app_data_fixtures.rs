//! Imports the app data fixtures in `tests/fixtures/app-data` and checks that every native field,
//! tag, namespace and blob reads back unchanged. The Swift and C# binding tests do the same.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use osaurus_sync_ffi::*;
use serde_json::Value;

#[derive(Default, Clone)]
struct Keys(Arc<Mutex<HashMap<String, Vec<u8>>>>);

impl KeyStore for Keys {
    fn load(&self, name: String) -> Result<Option<Vec<u8>>, SyncError> {
        Ok(self.0.lock().unwrap().get(&name).cloned())
    }
    fn store(&self, name: String, value: Vec<u8>) -> Result<(), SyncError> {
        self.0.lock().unwrap().insert(name, value);
        Ok(())
    }
    fn delete(&self, name: String) -> Result<(), SyncError> {
        self.0.lock().unwrap().remove(&name);
        Ok(())
    }
}

fn role(s: &str) -> MessageRole {
    match s {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System,
        "tool_call" => MessageRole::ToolCall,
        "tool_result" => MessageRole::ToolResult,
        other => panic!("unknown role {other}"),
    }
}

fn files(dir: &Path, list: Option<&Value>) -> Vec<AttachmentFile> {
    let Some(list) = list.and_then(Value::as_array) else { return vec![] };
    list.iter()
        .map(|a| {
            let name = a["name"].as_str().unwrap();
            let path = dir.join(format!("{}-{name}", NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::write(&path, a["text"].as_str().unwrap()).unwrap();
            AttachmentFile { path: path.to_string_lossy().into_owned(), media_type: a["media_type"].as_str().unwrap().into(), name: name.into() }
        })
        .collect()
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn object_bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap()
}

fn sorted_tags(v: &Value) -> Vec<Tag> {
    let mut tags: Vec<Tag> = v.as_object().unwrap().iter().map(|(k, v)| Tag { key: k.clone(), value: v.as_str().unwrap().into() }).collect();
    tags.sort_by(|a, b| a.key.cmp(&b.key));
    tags
}

#[test]
fn fixtures_round_trip() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/app-data");
    let dir = tempfile::tempdir().unwrap();
    let cfg = CoreConfig {
        data_dir: dir.path().join("core").to_string_lossy().into_owned(),
        device_name: "fixtures".into(),
        platform: Platform::MacOs,
        history: None,
        network: NetworkConfig { enabled: false, ..local_network_config() },
        max_app_data_bytes: None,
        api_version: api_version(),
    };
    let core = SyncCore::open(cfg, Box::new(Keys::default())).unwrap();
    let mut names: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).collect();
    names.sort();
    assert_eq!(names.len(), 5);
    for path in names {
        let f: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let c = &f["conversation"];
        let conv_data: Vec<AppDataEntry> =
            c["app_data"].as_object().unwrap().iter().map(|(ns, v)| AppDataEntry { namespace: ns.clone(), bytes: object_bytes(v) }).collect();
        let legacy = LegacyConversation {
            title: c["title"].as_str().unwrap().into(),
            created_ms: c["created_ms"].as_u64().unwrap(),
            replication: Replication::Synced,
            pinned: c["pinned"].as_bool().unwrap(),
            archived: c["archived"].as_bool().unwrap(),
            tags: sorted_tags(&c["tags"]),
            app_data: conv_data,
        };
        let msgs = f["messages"].as_array().unwrap();
        let legacy_msgs: Vec<LegacyMessage> = msgs
            .iter()
            .map(|m| LegacyMessage {
                role: role(m["role"].as_str().unwrap()),
                content: m["content"].as_str().unwrap().into(),
                created_ms: m["created_ms"].as_u64().unwrap(),
                attachments: files(dir.path(), m.get("attachments")),
                tool_call_id: m["tool_call_id"].as_str().map(Into::into),
                tool_name: m["tool_name"].as_str().map(Into::into),
                app_data: m["app_data"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(ns, v)| AppDataWrite {
                        namespace: ns.clone(),
                        bytes: object_bytes(v),
                        attachments: files(dir.path(), m.get("app_data_blobs").and_then(|b| b.get(ns))),
                    })
                    .collect(),
            })
            .collect();
        let scope = core.import_legacy_conversation(legacy.clone(), legacy_msgs).unwrap();

        let got = core.conversation(scope.clone()).unwrap().unwrap();
        assert_eq!(
            (got.title.as_str(), got.pinned, got.archived, got.created_ms),
            (legacy.title.as_str(), legacy.pinned, legacy.archived, legacy.created_ms),
            "{path:?}"
        );
        assert_eq!(got.tags, legacy.tags);
        assert_eq!(got.message_count as usize, msgs.len());
        for (ns, v) in c["app_data"].as_object().unwrap() {
            let e = got.app_data.iter().find(|e| &e.namespace == ns).unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&e.bytes).unwrap(), *v);
        }

        let page = core.messages(scope.clone(), None, 100).unwrap().messages;
        assert_eq!(page.len(), msgs.len());
        for (m, want) in page.iter().zip(msgs) {
            assert_eq!(m.content, want["content"].as_str().unwrap());
            assert_eq!(m.role, role(want["role"].as_str().unwrap()));
            assert_eq!(m.timestamp_ms, want["created_ms"].as_u64().unwrap());
            assert_eq!(m.tool_call_id.as_deref(), want["tool_call_id"].as_str());
            let atts = want.get("attachments").and_then(Value::as_array).map_or(0, Vec::len);
            assert_eq!(m.blobs.len(), atts);
            for (i, a) in want.get("attachments").and_then(Value::as_array).into_iter().flatten().enumerate() {
                assert_eq!(block_on(core.read_attachment(scope.clone(), m.blobs[i].hash.clone())).unwrap(), a["text"].as_str().unwrap().as_bytes());
            }
            for (ns, v) in want["app_data"].as_object().unwrap() {
                let d = m.app_data.iter().find(|d| &d.namespace == ns).unwrap_or_else(|| panic!("{ns} missing"));
                assert_eq!(serde_json::from_slice::<Value>(&d.bytes).unwrap(), *v);
                let blobs = want.get("app_data_blobs").and_then(|b| b.get(ns)).and_then(Value::as_array).cloned().unwrap_or_default();
                assert_eq!(d.blobs.len(), blobs.len());
                for (i, b) in blobs.iter().enumerate() {
                    assert_eq!(block_on(core.read_attachment(scope.clone(), d.blobs[i].hash.clone())).unwrap(), b["text"].as_str().unwrap().as_bytes());
                }
            }
        }
    }
    block_on(core.close()).unwrap();
}
