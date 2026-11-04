use super::*;

fn records(folder: &Path) -> (String, String) {
    let mut parts = vec![];
    for (id, second, failed) in [("alpha", 1, true), ("beta", 6, false)] {
        let at = format!("2026-10-20T00:00:{second:02}.000Z");
        let call = json!({"cwd":folder,"entrypoint":"cli","sessionId":"alpha","timestamp":at,
            "message":{"content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":"make release"}}]}});
        let result = json!({"cwd":folder,"entrypoint":"cli","sessionId":"alpha","timestamp":at,
            "message":{"content":[{"type":"tool_result","tool_use_id":id,"is_error":failed,
                "content":if failed {CLASSIFIER_REFUSAL} else {"beta"}}]}});
        parts.push(format!("{call}\n{result}\n"));
    }
    (parts.remove(0), parts.remove(0))
}

#[test]
fn replacement_and_truncation_discard_the_old_files_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path();
    let path = folder.join("alpha.jsonl");
    let (denied, passed) = records(folder);
    std::fs::write(&path, &denied).unwrap();
    let mut cursor = Cursor::default();
    assert_eq!(
        read(&path, folder, "alpha", &mut cursor)
            .unwrap()
            .denials
            .len(),
        1
    );
    let old_identity = cursor.identity;
    // The replacement is longer, so length alone cannot catch it.
    let replacement = format!("{passed}{}\n", " ".repeat(denied.len()));
    std::fs::write(folder.join("beta.tmp"), replacement).unwrap();
    std::fs::rename(folder.join("beta.tmp"), &path).unwrap();
    assert!(
        read(&path, folder, "alpha", &mut cursor)
            .unwrap()
            .denials
            .is_empty()
    );
    assert_ne!(cursor.identity, old_identity);
    std::fs::write(&path, &denied).unwrap();
    assert_eq!(
        read(&path, folder, "alpha", &mut cursor)
            .unwrap()
            .denials
            .len(),
        1
    );
    std::fs::write(&path, &passed).unwrap();
    assert!(
        read(&path, folder, "alpha", &mut cursor)
            .unwrap()
            .denials
            .is_empty()
    );
}

#[test]
fn a_busy_pane_is_read_in_bounded_steps_and_then_only_new_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path();
    let path = folder.join("alpha.jsonl");
    let (denied, passed) = records(folder);
    // An oversized API blob occupies more than one tick. It must not be
    // persisted, repeatedly parsed from the beginning, or block later calls.
    let blob = json!({"type":"api-request-blob","data":"x".repeat(STEP_BYTES as usize + 1024)});
    std::fs::write(&path, format!("{denied}{blob}\n{passed}")).unwrap();
    let mut cursor = Cursor::default();
    read(&path, folder, "alpha", &mut cursor).unwrap();
    assert!(cursor.read_bytes as u64 <= STEP_BYTES);
    let offset = cursor.end;
    read(&path, folder, "alpha", &mut cursor).unwrap();
    assert!(cursor.end > offset);
    assert!(cursor.read_bytes as u64 <= STEP_BYTES);
    let scan = read(&path, folder, "alpha", &mut cursor).unwrap();
    assert!(scan.denials.iter().any(|d| d["passed_ms"].is_i64()));
    assert_eq!(cursor.end, std::fs::metadata(&path).unwrap().len());
    read(&path, folder, "alpha", &mut cursor).unwrap();
    assert_eq!(cursor.read_bytes, 0);
    assert!(serde_json::to_string(&cursor).unwrap().len() < 4096);
}
