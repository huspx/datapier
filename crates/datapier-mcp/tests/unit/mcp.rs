use super::*;

#[test]
fn frozen_approval_cannot_contain_a_command() {
    let spec = SPECS
        .iter()
        .find(|spec| spec.name == "approve_command")
        .unwrap();
    let approval = Uuid::new_v4().to_string();
    let valid = json!({"approval_id":approval,"decision":"approve"});
    assert!(validate(spec, valid.as_object().unwrap()).is_ok());
    let injected =
        json!({"approval_id":approval,"decision":"approve","text":"DROP TABLE t","approved":true});
    assert!(validate(spec, injected.as_object().unwrap()).is_err());
}

#[test]
fn result_page_rejects_zero_and_non_uuid() {
    let spec = SPECS
        .iter()
        .find(|spec| spec.name == "read_result")
        .unwrap();
    assert!(validate(spec, json!({"result_id":"not-a-uuid"}).as_object().unwrap()).is_err());
    assert!(
        validate(
            spec,
            json!({"result_id":Uuid::new_v4().to_string(),"page":0})
                .as_object()
                .unwrap()
        )
        .is_err()
    );
    assert!(
        validate(
            spec,
            json!({"result_id":Uuid::new_v4().to_string(),"page":1})
                .as_object()
                .unwrap()
        )
        .is_ok()
    );
}

#[test]
fn database_diagnostics_are_structured_without_duplicating_rows() {
    let result = response(
        json!({"code":"database","message":"failed","database_code":"1064"}),
        true,
    );
    let CallToolResponse::Complete(result) = result else {
        panic!("expected completed tool result")
    };
    assert_eq!(result.structured_content.unwrap()["database_code"], "1064");
    assert_eq!(result.content.len(), 1);
    assert_eq!(result.is_error, Some(true));
}
