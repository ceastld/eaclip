use super::*;

fn send(title: &str, body: &str) -> Value {
    json!({"Type":"Send", "Title":title, "Group":"", "Data":{"Text":body}})
}

#[test]
fn text_actions_ui_tokens_cover_rule_changes_but_not_common_or_object_order() {
    let first: Value = serde_json::from_str(r#"{"Type":"Send","Data":{"Text":"hello","Custom":1},"Common":false}"#).unwrap();
    let reordered: Value = serde_json::from_str(r#"{"common":true,"Data":{"Custom":1,"Text":"hello"},"Type":"Send"}"#).unwrap();
    assert_eq!(rule_token(&first), rule_token(&reordered));
    let mut edited = first.clone();
    edited["Data"]["Custom"] = json!(2);
    assert_ne!(rule_token(&first), rule_token(&edited));
    edited = first.clone();
    edited["Title"] = json!("different action");
    assert_ne!(rule_token(&first), rule_token(&edited));
}

#[test]
fn text_actions_ui_star_only_changes_metadata_and_old_form_keeps_latest_star() {
    let mut actions = vec![json!({"Type":"Pair", "Title":"wrap", "Group":"coding", "Future":42,
        "Data":{"Prefix":"[", "Suffix":"]", "MultiLines":true,"Trim":true,"FutureParameter":"keep"}})];
    let original = actions[0].clone();
    let dto = text_actions::list_response(&actions)["actions"][0].clone();
    let token = dto["ruleToken"].as_str().unwrap();
    let starred = set_common(&mut actions, "0", token, true).unwrap();
    assert_eq!(starred["common"], true);
    assert_eq!(starred["ruleToken"], dto["ruleToken"]);
    let mut without_common = actions[0].clone();
    without_common.as_object_mut().unwrap().remove("Common");
    assert_eq!(without_common, original);

    // This form began before the star click. Even an explicitly stale common
    // value must not roll back the independent metadata on a rule save.
    let mut edit = dto;
    edit["expectedRuleToken"] = edit["ruleToken"].clone();
    edit["title"] = json!("renamed wrap");
    edit["common"] = json!(false);
    let saved = text_actions::upsert(&mut actions, &edit).unwrap();
    assert_eq!(saved["title"], "renamed wrap");
    assert_eq!(saved["common"], true);
    assert_eq!(saved["data"]["MultiLines"], true);
    assert_eq!(saved["data"]["Trim"], true);
    assert_eq!(saved["data"]["FutureParameter"], "keep");
}

#[test]
fn text_actions_ui_rejects_a_shifted_index_instead_of_targeting_its_new_rule() {
    let mut actions = vec![send("first", "one"), send("second", "two"), send("third", "three")];
    let token = rule_token(&actions[1]);
    text_actions::delete(&mut actions, "0").unwrap();
    let before = actions.clone();
    assert!(checked_action(&actions, "1", &token).is_err());
    assert!(set_common(&mut actions, "1", &token, true).is_err());
    assert!(text_actions::upsert(&mut actions, &json!({"id":"1", "expectedRuleToken":token,
        "type":"Send", "title":"wrong overwrite", "data":{"Text":"bad"}})).is_err());
    assert_eq!(actions, before);
    assert!(checked_action(&actions, "0", "").is_err());
}

#[test]
fn text_actions_ui_local_preview_uses_real_rules_including_pair_flags_and_escapes() {
    let pair = json!({"type":"Pair", "data":{"Prefix":"[", "Suffix":"]", "MultiLines":true, "Trim":true}});
    let result = preview(&pair, " a \n b ");
    assert_eq!(result["supported"], true);
    // Preserve the existing Trim contract: trim the whole source before
    // splitting lines; spaces inside individual lines remain significant.
    assert_eq!(result["output"], "[a ]\r\n[ b]");
    let replacement = json!({"type":"SingleReplace", "data":{"Pattern":"\\t", "EscapePattern":true,
        "ReplaceMent":"\\n", "EscapeReplaceMent":true, "UseRegex":false}});
    assert_eq!(preview(&replacement, "a\tb")["output"], "a\nb");
    let regex = json!({"Type":"SingleReplace", "Data":{"Pattern":"^a", "ReplaceMent":"b", "UseRegex":true,"Options":3}});
    assert_eq!(preview(&regex, "A\na")["output"], "b\nb");
    let empty = json!({"Type":"Internal", "Data":{"Type":"RemoveEmptyLine"}});
    assert_eq!(preview(&empty, "a\n\nb")["output"], "a\nb");
    let standard = json!({"Type":"Internal", "Data":{"Type":"StandardText"}});
    assert_eq!(preview(&standard, "  连 续 文 字  ")["output"], "连续文字");
    let insert = preview(&send("snippet", "insert this"), "ignored input");
    assert_eq!(insert["output"], "insert this");
    assert_eq!(insert["inputNeeded"], false);
    assert!(preview(&json!({"type":"Internal","data":{"Type":"FormatJson"}}), "invalid")["error"].is_string());
}

#[test]
fn text_actions_ui_preview_never_evaluates_expressions_or_launches_quicker() {
    for action in [
        json!({"Type":"ZExpression", "Data":{"Expresstion":"$={text}"}}),
        json!({"type":"ZExpression", "data":{"Expresstion":"$${[cliptext]}"}}),
        json!({"type":"QuickerAction", "data":{"Id":"must-not-launch"}}),
        json!({"type":"MultiReplace", "data":{}}),
    ] {
        let result = preview(&action, "private sample");
        assert_eq!(result["supported"], false);
        assert_eq!(result["output"], "");
        assert!(result["unsupportedReason"].as_str().is_some_and(|reason| !reason.is_empty()));
        assert!(result["error"].is_null());
    }
    let generate = json!({"Type":"ZExpression","Data":{"Expresstion":"$=DateTime.Now.Year"}});
    assert_eq!(preview(&generate, "")["inputNeeded"], false);
    assert_eq!(text_actions::list_response(&[generate])["actions"][0]["needInput"], false);
}

#[test]
fn text_actions_ui_filter_combines_purpose_literal_group_common_and_words() {
    let list = text_actions::list_response(&[
        json!({"Type":"Send","Title":"Mail signoff","Group":"common","Common":true,"Data":{"Text":"bye"}}),
        json!({"Type":"Pair","Title":"Code wrapper","Group":"all","Data":{"Prefix":"`","Suffix":"`"}}),
        json!({"Type":"ZExpression","Title":"Dynamic year","Group":"","Data":{"Expresstion":"$=DateTime.Now.Year"}}),
    ]);
    let actions = list["actions"].as_array().unwrap();
    assert_eq!(filter_actions(actions, "insert", Some("common"), true, "MAIL signoff").len(), 1);
    assert_eq!(filter_actions(actions, "transform", Some("all"), false, "wrapper").len(), 1);
    assert_eq!(filter_actions(actions, "external", Some(""), false, "year").len(), 1);
    assert!(filter_actions(actions, "transform", None, true, "").is_empty());
}

fn host_fixture() -> (tempfile::TempDir, crate::HostState, crate::AppPaths) {
    crate::clipboard::force_headless(true);
    let test_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&test_root).unwrap();
    let dir = tempfile::Builder::new().prefix("text-actions-ui-").tempdir_in(test_root).unwrap();
    let paths = crate::AppPaths::from_data_root(dir.path().join("text-actions-ui"));
    let host = crate::HostState::open(paths.clone(), 29065).unwrap();
    (dir, host, paths)
}

#[test]
fn text_actions_ui_host_persists_stars_and_checks_all_native_mutations() {
    let (_dir, host, paths) = host_fixture();
    let first = host.text_action_upsert(&json!({"type":"Send","title":"first","data":{"Text":"one"}})).unwrap();
    let second = host.text_action_upsert(&json!({"type":"Send","title":"second","data":{"Text":"two"}})).unwrap();
    let token = first["ruleToken"].as_str().unwrap();
    assert!(host.text_action_set_common(&json!({"id":"0","common":true})).is_err());
    host.text_action_set_common(&json!({"id":"0","common":true,"expectedRuleToken":token})).unwrap();
    let unchanged = host.text_action_snapshot_checked("0", token).unwrap();
    assert_eq!(unchanged["Title"], "first");
    assert_eq!(unchanged["Common"], true);
    drop(host);
    let host = crate::HostState::open(paths, 29065).unwrap();
    assert_eq!(host.text_action_list()["actions"][0]["common"], true);
    host.text_action_delete(&json!({"id":"0","expectedRuleToken":token})).unwrap();
    let before = host.text_action_list();
    assert!(host.text_action_set_common(&json!({"id":"0","common":false,"expectedRuleToken":token})).is_err());
    assert!(host.text_action_delete(&json!({"id":"0","expectedRuleToken":token})).is_err());
    assert_eq!(host.text_action_list(), before);
    assert_eq!(host.text_action_snapshot_checked("0", second["ruleToken"].as_str().unwrap()).unwrap()["Title"], "second");
}

#[test]
fn text_actions_ui_host_failed_star_save_keeps_memory_and_preview_has_no_side_effects() {
    let (_dir, host, paths) = host_fixture();
    let rule = host.text_action_upsert(&json!({"type":"Send","title":"snippet","data":{"Text":"body"}})).unwrap();
    let before = host.text_action_list();
    let path = paths.configs.join("TextActionData.json");
    let original = std::fs::read(&path).unwrap();
    let backup = path.with_extension("test-backup");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(host.text_action_set_common(&json!({"id":"0","common":true,"expectedRuleToken":rule["ruleToken"]})).is_err());
    assert_eq!(host.text_action_list(), before);
    assert_eq!(std::fs::read(&backup).unwrap(), original);
    let result = host.text_action_preview(&json!({"action":{"type":"ZExpression","data":{"Expresstion":"$${[cliptext]}"}},"input":"sample"})).unwrap();
    assert_eq!(result["supported"], false);
    assert_eq!(host.text_action_list(), before);
    assert!(host.text_action_preview(&json!({"action":null})).is_err());
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
}
