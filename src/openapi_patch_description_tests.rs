use super::*;

#[test]
fn openapi_patch_request_descriptions_reject_codex_wrapper() {
    let spec = build_openapi_spec();
    let schemas = &spec["components"]["schemas"];
    let apply_desc = schemas["ApplyPatchRequest"]["properties"]["patch"]["description"]
        .as_str()
        .expect("ApplyPatchRequest patch description");
    let validate_desc = schemas["ValidatePatchRequest"]["properties"]["patch"]["description"]
        .as_str()
        .expect("ValidatePatchRequest patch description");
    let checked_desc = schemas["ApplyPatchCheckedRequest"]["properties"]["patch"]["description"]
        .as_str()
        .expect("ApplyPatchCheckedRequest patch description");

    assert!(
        apply_desc.contains("raw standard unified diff"),
        "{apply_desc}"
    );
    assert!(
        validate_desc.contains("Codex apply_patch wrapper"),
        "{validate_desc}"
    );
    assert!(checked_desc.contains("*** Begin Patch"), "{checked_desc}");
}
