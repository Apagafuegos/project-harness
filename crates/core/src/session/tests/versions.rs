//! Record versions: per-version schemas, the writer's version, the monotonic
//! rule, the storage rules on v2 fields, and v1 journals continued as v2.

use super::*;

/// SHA-256 of `session-v1.schema.json`. v1 journals exist, so its bytes never change.
const V1_SCHEMA_SHA256: &str = "7fb670f5adff34d2f8ef53305afe4662d5c6425b81ef1e1dca137db490cefbca";
/// SHA-256 of `session-v2.schema.json`. v2 journals exist, so its bytes never change either.
const V2_SCHEMA_SHA256: &str = "64ecdfd6e2eaab57272cf1e223398a1d5a726ec512633b9ecd9a67ffb6eeb2b7";

/// One framed line for `record`; `decode` checks the checksum, not the chain.
fn line(record: &Value) -> Vec<u8> {
    let raw = serde_json::to_string(record).unwrap();
    format!(
        "{{\"record\":{raw},\"sha256\":\"{}\"}}",
        Digest::of(raw.as_bytes())
    )
    .into_bytes()
}

fn decoded(record: &Value) -> std::result::Result<Record, &'static str> {
    match codec::decode(&line(record)) {
        Ok((record, _)) => Ok(record),
        Err(StoreError::Invalid(code)) => Err(code),
        Err(error) => panic!("{error}"),
    }
}

/// The records of one complete authorized tool turn written by this build.
fn current_records() -> Vec<Value> {
    let (_temp, root, mut store) = fresh();
    append_workload(&root, &mut store, Workload::ToolTurns, TOOL_TURN_COMMITS);
    records(&fs::read(journal_path(&root, store.snapshot().session_id())).unwrap())
}

/// The first record holding an entry or event of `kind`, and its JSON path.
fn holding(records: &[Value], kind: &str) -> (Value, &'static str, usize) {
    for record in records {
        for (list, field) in [("entries", "payload"), ("events", "")] {
            let items = record["payload"][list].as_array().into_iter().flatten();
            for (index, item) in items.enumerate() {
                let item = if field.is_empty() { item } else { &item[field] };
                if item["kind"] == kind {
                    return (record.clone(), list, index);
                }
            }
        }
    }
    panic!("no {kind}")
}

/// The object holding the v2 field of a record from `holding`.
fn field_owner<'a>(record: &'a mut Value, kind: &str, list: &str, index: usize) -> &'a mut Value {
    let item = &mut record["payload"][list][index];
    match kind {
        "tool_result" => &mut item["payload"]["result"]["metadata"],
        "assistant_response" => &mut item["payload"],
        _ => item,
    }
}

/// `(kind, field, first version that requires it)`.
const FIELDS: [(&str, &str, u32); 5] = [
    ("tool_result", "code", 2),
    ("execution_intent", "effects", 2),
    ("assistant_response", "projection_fingerprint", 2),
    ("tool_result", "file_effect", 4),
    ("execution_intent", "file_image", 4),
];

#[test]
fn shipped_schema_bytes_are_pinned() {
    let bytes = include_bytes!("../../../../../docs/rfcs/001/schema/session-v1.schema.json");
    assert_eq!(Digest::of(bytes).as_str(), V1_SCHEMA_SHA256);
    let bytes = include_bytes!("../../../../../docs/rfcs/001/schema/session-v2.schema.json");
    assert_eq!(Digest::of(bytes).as_str(), V2_SCHEMA_SHA256);
}

#[test]
fn only_integer_supported_versions_select_a_schema() {
    // A header has no versioned field, so it is a valid record of each version.
    let header = records(&source("branching")).remove(0);
    for version in [json!(1), json!(2), json!(3), json!(4), json!(5)] {
        let mut record = header.clone();
        record["version"] = version.clone();
        assert_eq!(
            decoded(&record).unwrap().version,
            version.as_u64().unwrap() as u32
        );
    }
    for version in [
        json!(0),
        json!(6),
        json!("2"),
        json!(2.0),
        json!(-1),
        json!(null),
    ] {
        let mut record = header.clone();
        record["version"] = version.clone();
        assert_eq!(
            decoded(&record).unwrap_err(),
            "unsupported_version",
            "{version}"
        );
    }
    let mut record = header;
    record.as_object_mut().unwrap().remove("version");
    assert_eq!(decoded(&record).unwrap_err(), "unsupported_version");
}

#[test]
fn each_version_requires_exactly_its_own_fields() {
    let records = current_records();
    for (kind, field, requires) in FIELDS {
        let (current, list, index) = holding(&records, kind);
        decoded(&current).unwrap();
        // The key is unknown in the version before the one that requires it.
        let mut older = current.clone();
        older["version"] = json!(requires - 1);
        for (other, other_field, other_requires) in FIELDS {
            if other == kind && other_field != field && other_requires > requires - 1 {
                let owner = field_owner(&mut older, other, list, index);
                if let Some(map) = owner.as_object_mut() {
                    map.remove(other_field);
                }
            }
        }
        assert_eq!(decoded(&older).unwrap_err(), "schema_violation", "{kind}");
        // Without the key, its own version is missing a required field.
        let mut without = current.clone();
        field_owner(&mut without, kind, list, index)
            .as_object_mut()
            .unwrap()
            .remove(field)
            .unwrap();
        assert_eq!(decoded(&without).unwrap_err(), "schema_violation", "{kind}");
        // The previous version, which lacks every later field, is valid.
        without["version"] = json!(requires - 1);
        for (other, other_field, other_requires) in FIELDS {
            if other == kind && other_field != field && other_requires > requires - 1 {
                let owner = field_owner(&mut without, other, list, index);
                if let Some(map) = owner.as_object_mut() {
                    map.remove(other_field);
                }
            }
        }
        decoded(&without).unwrap();
    }
}

/// The first record of `name` holding an entry of `kind`, decoded from fixture bytes.
fn entry_record(name: &str, kind: &str) -> Value {
    records(&source(name))
        .into_iter()
        .find(|record| {
            record["payload"]["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|entry| entry["payload"]["kind"] == kind)
        })
        .unwrap_or_else(|| panic!("{name} has no {kind}"))
}

#[test]
fn a_v3_compaction_may_omit_its_model_but_never_write_null() {
    let summary = entry_record("compaction", "compaction");
    let mut record = summary.clone();
    record["version"] = json!(3);
    decoded(&record).unwrap();
    let mut without = record.clone();
    without["payload"]["entries"][0]["payload"]
        .as_object_mut()
        .unwrap()
        .remove("model")
        .unwrap();
    let parsed = decoded(&without).unwrap();
    let RecordPayload::Commit(commit) = parsed.payload else {
        panic!("a commit")
    };
    let EntryPayload::Compaction(compaction) = &commit.entries[0].payload else {
        panic!("a compaction")
    };
    assert_eq!(compaction.model, None, "absent means no model was used");
    // Null is not a way to say "no model"; the field is present or absent.
    let mut null = without.clone();
    null["payload"]["entries"][0]["payload"]["model"] = Value::Null;
    assert_eq!(decoded(&null).unwrap_err(), "schema_violation");
    // v2 and v1 still require it.
    for version in [1, 2] {
        let mut older = without.clone();
        older["version"] = json!(version);
        assert_eq!(
            decoded(&older).unwrap_err(),
            "schema_violation",
            "v{version}"
        );
    }
}

#[test]
fn context_control_is_a_v3_entry_kind_with_a_closed_mode_set() {
    let record = entry_record("context-controls", "context_control");
    assert_eq!(record["version"], 3);
    decoded(&record).unwrap();
    for mode in ["pinned", "excluded", "default"] {
        let mut value = record.clone();
        value["payload"]["entries"][0]["payload"]["mode"] = json!(mode);
        decoded(&value).unwrap();
    }
    // v1 and v2 do not know the kind.
    for version in [1, 2] {
        let mut older = record.clone();
        older["version"] = json!(version);
        assert_eq!(
            decoded(&older).unwrap_err(),
            "schema_violation",
            "v{version}"
        );
    }
    // The mode set is closed, and the target turn is required.
    let mut unknown = record.clone();
    unknown["payload"]["entries"][0]["payload"]["mode"] = json!("favourite");
    assert_eq!(decoded(&unknown).unwrap_err(), "schema_violation");
    let mut no_turn = record;
    no_turn["payload"]["entries"][0]["payload"]
        .as_object_mut()
        .unwrap()
        .remove("turn_id")
        .unwrap();
    assert_eq!(decoded(&no_turn).unwrap_err(), "schema_violation");
}

#[test]
fn writers_refuse_any_version_but_the_current_one() {
    let (record, _) = codec::decode(&line(&records(&source("branching"))[0])).unwrap();
    for version in [1, 2, 3] {
        let record = Record {
            version,
            ..record.clone()
        };
        assert!(matches!(
            codec::encode(&record),
            Err(StoreError::Invalid("write_version"))
        ));
    }
    codec::encode(&Record {
        version: CURRENT_VERSION,
        ..record
    })
    .unwrap();
}

#[test]
fn new_sessions_write_every_record_as_v5() {
    assert_eq!(
        CURRENT_VERSION, 5,
        "the writer's version is the record format"
    );
    let records = current_records();
    assert_eq!(records.len(), TOOL_TURN_COMMITS + 1);
    assert!(records.iter().all(|r| r["version"] == json!(5)));
    let (intent, _, index) = holding(&records, "execution_intent");
    assert_eq!(
        intent["payload"]["events"][index]["effects"],
        json!(["read"])
    );
    // A read intent binds no image; the v4 key is present and null.
    assert_eq!(
        intent["payload"]["events"][index]["file_image"],
        Value::Null
    );
}

#[test]
fn code_and_effects_values_are_checked_by_the_schema() {
    let records = current_records();
    let (result, list, index) = holding(&records, "tool_result");
    let longest = "a".repeat(64);
    for (code, valid) in [
        (json!("tools_unavailable"), true),
        (json!(longest), true),
        (json!(null), true),
        (json!("Tools_unavailable"), false),
        (json!("1code"), false),
        (json!("_code"), false),
        (json!(""), false),
        (json!("a".repeat(65)), false),
        (json!("code\n"), false),
        (json!("code-name"), false),
        (json!(1), false),
    ] {
        let mut record = result.clone();
        field_owner(&mut record, "tool_result", list, index)["code"] = code.clone();
        assert_eq!(decoded(&record).is_ok(), valid, "{code}");
        if !valid {
            assert_eq!(decoded(&record).unwrap_err(), "schema_violation");
        }
    }
    let (intent, list, index) = holding(&records, "execution_intent");
    for effects in [
        json!([]),
        json!(["read", "read"]),
        json!(["delete"]),
        json!("read"),
        json!(null),
    ] {
        let mut record = intent.clone();
        field_owner(&mut record, "execution_intent", list, index)["effects"] = effects.clone();
        assert_eq!(
            decoded(&record).unwrap_err(),
            "schema_violation",
            "{effects}"
        );
    }
    // Order is a storage rule, not a schema one.
    let mut record = intent;
    field_owner(&mut record, "execution_intent", list, index)["effects"] =
        json!(["process", "read"]);
    decoded(&record).unwrap();
}

#[test]
fn effect_sets_are_constructed_canonical() {
    use EffectClass::*;
    let set = EffectSet::new([Network, Read, Process, Read]).unwrap();
    assert_eq!(set.as_slice(), [Read, Process, Network]);
    assert!(set.is_canonical());
    assert_eq!(
        serde_json::to_value(&set).unwrap(),
        json!(["read", "process", "network"])
    );
    assert!(EffectSet::new([]).is_none());
    let stored: EffectSet = serde_json::from_value(json!(["process", "read"])).unwrap();
    assert_eq!(
        stored.as_slice(),
        [Process, Read],
        "a decoded set keeps its order"
    );
    assert!(!stored.is_canonical());
}

#[path = "recovery.rs"]
mod recovery;

/// A store in `authorizing` with one `read` call pending, and that call.
fn authorizing() -> (tempfile::TempDir, Arc<DataRoot>, JournalStore, CallRef) {
    let (temp, root, mut store) = fresh();
    append_workload(&root, &mut store, Workload::ToolTurns, 4);
    let snapshot = store.snapshot();
    assert_eq!(snapshot.state(), RuntimeState::Authorizing);
    let call = snapshot.0.pending(snapshot.head()).unwrap()[0].clone();
    (temp, root, store, call)
}

/// An allow decision and the intent it authorizes, in one commit.
fn intent(
    root: &DataRoot,
    snapshot: &SessionSnapshot,
    call: &CallRef,
    backend: ExecutionBackend,
    effects: EffectSet,
) -> Commit {
    let turn_id = snapshot.active_turn().unwrap().clone();
    let mutation =
        backend == ExecutionBackend::TrustedFs && effects.contains(EffectClass::FilesystemMutation);
    let image = mutation.then(|| {
        let pre = root.put_artifact(b"before").unwrap();
        let post = Digest::of(b"after");
        let bytes = serde_json::to_vec(&json!({
            "version": 2,
            "name": "write",
            "workspace": "/workspace",
            "target": {"path": "f0", "pre_image": pre.sha256, "content_sha256": post},
        }))
        .unwrap();
        (
            root.put_artifact(&bytes).unwrap(),
            crate::session::FileImage {
                path: "f0".into(),
                pre_image: Some(pre),
                pre_mode: Some(0o644),
                post_image: post,
                additional_targets: Vec::new(),
            },
        )
    });
    let (plan, file_image) = match image {
        Some((plan, image)) => (plan, Some(image)),
        None => (root.put_artifact(br#"{"plan":"effects"}"#).unwrap(), None),
    };
    let decision_id = DecisionId::new();
    audit(
        snapshot,
        vec![
            JournalEvent::AuthorizationDecided(AuthorizationDecided {
                turn_id: turn_id.clone(),
                decision_id: decision_id.clone(),
                call: call.clone(),
                approval_id: None,
                plan_digest: plan.sha256.clone(),
                plan: plan.clone(),
                policy_revision: snapshot.policy_revision().into(),
                decision: Decision::Allow,
                reason: "effects test".into(),
            }),
            JournalEvent::ExecutionIntent(ExecutionIntent {
                turn_id,
                invocation_id: InvocationId::new(),
                call: call.clone(),
                decision_id,
                plan_digest: plan.sha256,
                backend,
                timeout_ms: 1_000,
                effects: Some(effects),
                file_image,
            }),
        ],
    )
}

#[test]
fn intent_effects_are_canonical_and_possible_for_the_backend() {
    use EffectClass::*;
    use ExecutionBackend::*;
    let stored = |effects: Value| serde_json::from_value::<EffectSet>(effects).unwrap();
    let set = |effects: &[EffectClass]| EffectSet::new(effects.iter().copied()).unwrap();
    for (backend, effects, expected) in [
        (
            Bubblewrap,
            stored(json!(["process", "read"])),
            Some("effects_not_canonical"),
        ),
        (
            TrustedFs,
            stored(json!(["filesystem_mutation", "read"])),
            Some("effects_not_canonical"),
        ),
        (
            TrustedFs,
            set(&[Read, Process]),
            Some("effects_backend_mismatch"),
        ),
        (TrustedFs, set(&[Network]), Some("effects_backend_mismatch")),
        (
            TrustedFs,
            set(&[ExternalSideEffect]),
            Some("effects_backend_mismatch"),
        ),
        (Bubblewrap, set(&[Read]), Some("effects_backend_mismatch")),
        (
            Bubblewrap,
            set(&[Read, FilesystemMutation]),
            Some("effects_backend_mismatch"),
        ),
        (TrustedFs, set(&[Read]), None),
        (TrustedFs, set(&[Read, FilesystemMutation]), None),
        (Bubblewrap, set(&[Process]), None),
        (
            Bubblewrap,
            set(&[
                Read,
                FilesystemMutation,
                Process,
                Network,
                ExternalSideEffect,
            ]),
            None,
        ),
    ] {
        let (_temp, root, mut store, call) = authorizing();
        let before = store.snapshot();
        let result = store.append(
            intent(&root, &before, &call, backend, effects.clone()),
            at(),
        );
        match expected {
            Some(code) => {
                assert!(
                    matches!(result, Err(StoreError::Invalid(c)) if c == code),
                    "{backend:?} {effects:?}: {result:?}"
                );
                assert_eq!(store.snapshot().checksum(), before.checksum());
            }
            None => {
                result.unwrap();
                let id = store.snapshot().session_id().clone();
                drop(store);
                let reopened = JournalStore::open(root, &id).unwrap().snapshot();
                let [unresolved] = &reopened.unresolved_invocations()[..] else {
                    panic!("one intent")
                };
                assert_eq!(unresolved.intent.effects.as_ref(), Some(&effects));
            }
        }
    }
}

#[test]
fn v2_results_carry_a_code_exactly_when_the_system_failed() {
    use Provenance::*;
    use ToolStatus::*;
    // After the response: `processing`, with its one call pending.
    let (_temp, root, mut store) = fresh();
    append_workload(&root, &mut store, Workload::ToolTurns, 3);
    let snapshot = store.snapshot();
    let call = snapshot.0.pending(snapshot.head()).unwrap()[0].clone();
    for (status, provenance, code, expected) in [
        (Succeeded, Runtime, Some("done"), "code_on_success"),
        (Succeeded, Executor, Some("done"), "code_on_success"),
        (Failed, Runtime, None, "code_required"),
        (Denied, Runtime, None, "code_required"),
        (Cancelled, Executor, None, "code_required"),
        (
            Failed,
            UserObserved,
            Some("process_restart"),
            "code_on_user_result",
        ),
        (
            Uncertain,
            UserAcknowledgedUnknown,
            Some("process_restart"),
            "code_on_user_result",
        ),
    ] {
        let mut commit = tool_result(&snapshot, call.clone(), None, status, provenance);
        let EntryPayload::ToolResult(result) = &mut commit.entries[0].payload else {
            unreachable!()
        };
        result.result.metadata.code = code.map(Into::into);
        assert!(
            matches!(store.append(commit, at()), Err(StoreError::Invalid(c)) if c == expected),
            "{status:?} {provenance:?} {code:?}"
        );
    }
    let mut denied = runtime_result(&snapshot, call);
    let EntryPayload::ToolResult(result) = &mut denied.entries[0].payload else {
        unreachable!()
    };
    result.result.metadata.code = Some("tools_unavailable".into());
    store.append(denied, at()).unwrap();
}

#[test]
fn a_lower_version_after_a_higher_one_is_corrupt() {
    let (_temp, root, mut store) = fresh();
    store
        .append(start_turn(&store.snapshot(), "one"), at())
        .unwrap();
    fail_turn(&mut store);
    let id = store.snapshot().session_id().clone();
    drop(store);
    let path = journal_path(&root, &id);
    let mut values = records(&fs::read(&path).unwrap());
    // Each record is a valid v1 shape, and the rebuilt chain is intact.
    values[2]["version"] = json!(1);
    let bytes = reframe(&values);
    fs::write(&path, &bytes).unwrap();
    let inspection = JournalStore::inspect(&root, &id).unwrap();
    let prefix = bytes
        .split_inclusive(|b| *b == b'\n')
        .take(2)
        .map(<[u8]>::len)
        .sum::<usize>();
    assert_eq!(
        inspection.diagnostic,
        Some(Diagnostic {
            offset: prefix as u64,
            kind: DiagnosticKind::Corrupt,
            code: "version_regression".into(),
        })
    );
    assert_eq!(inspection.versions, [(5, 2)].into());
    // Equal or higher is fine: v1, v1, v5 replays.
    values[0]["version"] = json!(1);
    values[1]["version"] = json!(1);
    values[2]["version"] = json!(5);
    fs::write(&path, reframe(&values)).unwrap();
    let inspection = JournalStore::inspect(&root, &id).unwrap();
    assert_eq!(inspection.diagnostic, None);
    assert_eq!(inspection.versions, [(1, 2), (5, 1)].into());
}

/// The versions of a journal's records, in order.
fn versions(root: &DataRoot, id: &SessionId) -> Vec<u64> {
    records(&fs::read(journal_path(root, id)).unwrap())
        .iter()
        .map(|r| r["version"].as_u64().unwrap())
        .collect()
}

#[test]
fn a_repaired_v1_tail_continues_as_v5() {
    let (_temp, root, id) = fixture("compaction");
    let v1 = records(&source("compaction")).len();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(journal_path(&root, &id))
        .unwrap();
    file.write_all(br#"{"record":{"version":1,"seq":"#).unwrap();
    drop(file);
    let inspection = JournalStore::inspect(&root, &id).unwrap();
    assert_eq!(
        inspection.diagnostic.unwrap().kind,
        DiagnosticKind::TornTail
    );
    assert_eq!(inspection.versions, [(1, v1 as u64)].into());
    let mut store = JournalStore::repair_inspected_tail(
        root.clone(),
        &id,
        &inspection.fingerprint,
        inspection.file_bytes,
        at(),
    )
    .unwrap();
    store
        .append(start_turn(&store.snapshot(), "after repair"), at())
        .unwrap();
    fail_turn(&mut store);
    let live = store.snapshot();
    drop(store);
    let reopened = JournalStore::open(root.clone(), &id).unwrap().snapshot();
    assert_eq!(reopened.checksum(), live.checksum());
    let mut expected = vec![1; v1];
    expected.extend([5, 5, 5]);
    assert_eq!(versions(&root, &id), expected, "the repair commit is v5");
    let inspection = JournalStore::inspect(&root, &id).unwrap();
    assert_eq!(inspection.diagnostic, None);
    assert_eq!(inspection.versions, [(1, v1 as u64), (5, 3)].into());
}
