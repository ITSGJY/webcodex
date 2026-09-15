use super::*;
use serde_json::json;

fn variant_for_kind<'a>(schema: &'a Value, kind: &str) -> &'a Value {
    schema["oneOf"]
        .as_array()
        .expect("discriminated schema oneOf")
        .iter()
        .find(|variant| variant["properties"]["kind"]["enum"] == json!([kind]))
        .unwrap_or_else(|| panic!("missing schema variant {kind}"))
}

fn apply_text_edits_schema_accepts(schema: &Value, value: &Value) -> bool {
    test_support::validate_schema_instance(value, schema).is_ok()
}

fn schema_accepts(schema: &Value, value: &Value) -> bool {
    test_support::validate_schema_instance(value, schema).is_ok()
}

#[test]
fn apply_text_edits_input_schema_encodes_file_and_edit_kind_contracts() {
    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "apply_text_edits");
    let schema = &spec.input_schema;
    let changes = &schema["properties"]["changes"];

    assert_eq!(changes["type"], "array");
    assert_eq!(changes["minItems"], 1);
    assert_eq!(changes["maxItems"], 16);
    assert_eq!(changes["items"]["oneOf"].as_array().unwrap().len(), 4);

    let edit = variant_for_kind(&changes["items"], "edit");
    let create = variant_for_kind(&changes["items"], "create");
    let delete = variant_for_kind(&changes["items"], "delete");
    let rename = variant_for_kind(&changes["items"], "rename");
    assert_eq!(edit["required"], json!(["kind", "path", "edits"]));
    assert_eq!(create["required"], json!(["kind", "path", "content"]));
    assert_eq!(
        delete["required"],
        json!(["kind", "path", "expected_read_revision"])
    );
    assert_eq!(
        rename["required"],
        json!(["kind", "path", "to_path", "expected_read_revision"])
    );
    assert_eq!(
        edit["properties"]["expected_read_revision"]["type"],
        "integer"
    );
    assert_eq!(
        edit["properties"]["expected_read_revision"]["maximum"],
        9007199254740991_u64
    );
    for variant in [edit, create, delete, rename] {
        assert_eq!(variant["additionalProperties"], false);
        assert_eq!(variant["properties"]["path"]["minLength"], 1);
    }

    let edit_items = &edit["properties"]["edits"]["items"];
    assert_eq!(edit_items["oneOf"].as_array().unwrap().len(), 4);
    let replace = variant_for_kind(edit_items, "replace_exact");
    let remove = variant_for_kind(edit_items, "delete_exact");
    let insert_before = variant_for_kind(edit_items, "insert_before");
    let insert_after = variant_for_kind(edit_items, "insert_after");
    assert_eq!(replace["required"], json!(["kind", "old_text"]));
    assert_eq!(remove["required"], json!(["kind", "old_text"]));
    assert_eq!(
        insert_before["required"],
        json!(["kind", "anchor_text", "new_text"])
    );
    assert_eq!(
        insert_after["required"],
        json!(["kind", "anchor_text", "new_text"])
    );
    assert_eq!(replace["properties"]["old_text"]["minLength"], 1);
    assert_eq!(remove["properties"]["old_text"]["minLength"], 1);
    assert!(replace["properties"].get("anchor_text").is_none());
    assert!(remove["properties"].get("new_text").is_none());
    assert!(remove["properties"].get("anchor_text").is_none());
    for insert in [insert_before, insert_after] {
        assert_eq!(insert["properties"]["anchor_text"]["minLength"], 1);
        assert!(insert["properties"]["new_text"].get("minLength").is_none());
        assert!(insert["properties"]["new_text"]["description"]
            .as_str()
            .unwrap()
            .contains("no-op"));
        assert!(insert["properties"].get("old_text").is_none());
    }
    assert!(replace["properties"]["new_text"].get("minLength").is_none());
    for variant in [replace, remove, insert_before, insert_after] {
        let line_scope = &variant["properties"]["line_scope"];
        assert_eq!(line_scope["type"], "object");
        assert_eq!(line_scope["additionalProperties"], false);
        assert_eq!(line_scope["required"], json!(["start_line", "end_line"]));
        assert_eq!(line_scope["properties"]["start_line"]["minimum"], 1);
        assert_eq!(line_scope["properties"]["end_line"]["minimum"], 1);
        assert!(line_scope["description"]
            .as_str()
            .unwrap()
            .contains("global source-order"));
    }

    let revision = 3817291045227_u64;
    let valid = [
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","edits":[{"kind":"replace_exact","old_text":"old"}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","edits":[{"kind":"delete_exact","old_text":"old"}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","edits":[{"kind":"insert_before","anchor_text":"anchor","new_text":"new"}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","expected_read_revision":revision,"edits":[{"kind":"replace_exact","old_text":"old"}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","expected_read_revision":revision,"edits":[{"kind":"replace_exact","old_text":"old","line_scope":{"start_line":10,"end_line":20}}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","expected_read_revision":revision,"edits":[{"kind":"insert_after","anchor_text":"anchor","new_text":"new","occurrence":2}]}]}),
        json!({"project":"demo","changes":[{"kind":"create","path":"new.txt","content":""}]}),
        json!({"project":"demo","changes":[{"kind":"delete","path":"old.txt","expected_read_revision":revision}]}),
        json!({"project":"demo","changes":[{"kind":"rename","path":"old.txt","to_path":"new.txt","expected_read_revision":revision}]}),
    ];
    for value in valid {
        assert!(
            apply_text_edits_schema_accepts(schema, &value),
            "valid apply_text_edits shape rejected: {value}"
        );
    }

    let invalid = [
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","edits":[{"kind":"replace_exact","old_text":"old","anchor_text":"anchor"}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","edits":[{"kind":"replace_exact"}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","edits":[{"kind":"delete_exact","old_text":"old","new_text":"x"}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","edits":[{"kind":"insert_after","anchor_text":"anchor"}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","edits":[{"kind":"replace_exact","old_text":"old","line_scope":{"start_line":10,"end_line":20}}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","edits":[{"kind":"replace_exact","old_text":"old","occurrence":2}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","expected_read_revision":0,"edits":[{"kind":"replace_exact","old_text":"old"}]}]}),
        json!({"project":"demo","changes":[{"kind":"edit","path":"a.rs","expected_read_revision":9007199254740992_u64,"edits":[{"kind":"replace_exact","old_text":"old"}]}]}),
        json!({"project":"demo","changes":[{"kind":"create","path":"new.txt","content":"x","expected_read_revision":revision}]}),
        json!({"project":"demo","changes":[{"kind":"delete","path":"old.txt"}]}),
        json!({"project":"demo","changes":[{"kind":"rename","path":"old.txt","to_path":"new.txt"}]}),
    ];
    for value in invalid {
        assert!(
            !apply_text_edits_schema_accepts(schema, &value),
            "invalid apply_text_edits shape accepted: {value}"
        );
    }
}

#[test]
fn write_project_file_schema_uses_read_revision_for_whole_file_replacement() {
    let specs = registered_tool_specs();
    let schema = &spec_named(&specs, "write_project_file").input_schema;
    let revision = 3817291045227_u64;

    assert_eq!(
        schema["properties"]["expected_read_revision"]["type"],
        "integer"
    );
    assert_eq!(
        schema["properties"]["expected_read_revision"]["maximum"],
        9007199254740991_u64
    );
    assert!(schema["properties"].get("expected_sha256").is_none());

    for value in [
        json!({"project":"demo","path":"new.rs","content":"fn main() {}"}),
        json!({"project":"demo","path":"existing.rs","content":"fn main() {}","overwrite":true,"expected_read_revision":revision}),
    ] {
        assert!(
            schema_accepts(schema, &value),
            "valid write shape rejected: {value}"
        );
    }
    for value in [
        json!({"project":"demo","path":"existing.rs","content":"x","overwrite":true}),
        json!({"project":"demo","path":"existing.rs","content":"x","expected_read_revision":revision}),
        json!({"project":"demo","path":"existing.rs","content":"x","overwrite":false,"expected_read_revision":revision}),
        json!({"project":"demo","path":"existing.rs","content":"x","overwrite":true,"expected_read_revision":0}),
        json!({"project":"demo","path":"existing.rs","content":"x","overwrite":true,"expected_read_revision":9007199254740992_u64}),
        json!({"project":"demo","path":"existing.rs","content":"x","overwrite":true,"expected_sha256":"a".repeat(64)}),
    ] {
        assert!(
            !schema_accepts(schema, &value),
            "invalid write shape accepted: {value}"
        );
    }
}
