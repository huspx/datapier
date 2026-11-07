use super::*;

// 独立解析此处使用的 POSIX 单引号、未引用反斜线及空白规则。
fn posix_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = text.chars();
    let mut quoted = false;
    let mut active = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                quoted = !quoted;
                active = true;
            }
            '\\' if !quoted => {
                word.push(chars.next().expect("shell escape"));
                active = true;
            }
            c if c.is_whitespace() && !quoted => {
                if active {
                    words.push(std::mem::take(&mut word));
                    active = false;
                }
            }
            c => {
                word.push(c);
                active = true;
            }
        }
    }
    assert!(!quoted);
    if active {
        words.push(word);
    }
    words
}

fn response(columns: Json, types: Json, rows: Json) -> QueryEnvelope {
    serde_json::from_value(json!({"status":0,"msg":"ok","data":{"column_list":columns,"column_type":types,"rows":rows,"affected_rows":2,"is_masked":false,"mask_rule_hit":false,"full_sql":"select 1","error":null}})).unwrap()
}

fn cache_session(provider: ProviderKind) -> ArcherySession {
    use crate::model::{
        AccessMode, ArcheryConnection, ArcheryServer, ConnectionConfig, HttpTls, Profile,
    };
    use std::sync::Arc;
    use uuid::Uuid;
    let server = ArcheryServer {
        id: Uuid::new_v4(),
        name: "fixture".into(),
        base_url: "http://127.0.0.1/".into(),
        username: "test".into(),
        http_tls: HttpTls::default(),
        totp_mode: Default::default(),
        revision: 1,
    };
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "fixture".into(),
        revision: 1,
        approval_policy: Default::default(),
        provider,
        access_mode: AccessMode::ReadOnly,
        connection: ConnectionConfig::Archery(ArcheryConnection {
            server_id: server.id,
            instance_id: 1,
            instance_name: "instance".into(),
            databases: vec!["0".into()],
            default_database: "0".into(),
        }),
    };
    ArcherySession {
        capabilities: profile.capabilities(),
        profile,
        auth: Arc::new(super::super::AuthSlot::new(server).unwrap()),
        database: "0".into(),
        closed: false,
    }
}

#[tokio::test]
async fn cached_platform_responses_preserve_columns_values_and_redis_coverage() {
    use crate::Store;
    use uuid::Uuid;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let root = std::env::temp_dir().join(format!("datapier-archery-cache-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let task = store
        .create_task(
            Uuid::new_v4(),
            "archery-test",
            "execution.start",
            json!({}),
            false,
        )
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
    let cancel = CancellationToken::new();
    let mysql = cache_session(ProviderKind::Mysql);
    let empty = decode_query(
        response(json!(["x", "x"]), json!(["LONG", "VARCHAR"]), json!([])),
        false,
    )
    .unwrap();
    let id = cache_response(&mysql, "0", empty, 10, &sink, &cancel)
        .await
        .unwrap()["results"][0]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let meta = store.result_meta(id).await.unwrap();
    assert_eq!(
        meta.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["x", "x"]
    );
    assert!(
        store
            .result_page(id, 0, 10, 16384)
            .await
            .unwrap()
            .rows
            .is_empty()
    );

    let envelope: QueryEnvelope = serde_json::from_str(r#"{"status":0,"data":{"column_list":["x","x","unknown"],"column_type":["LONGLONG","NEWDECIMAL","CUSTOM"],"rows":[[9007199254740993,12345678901234567890.123456789,9876543210123456789],[2,"0.000000000000000001",null]],"affected_rows":2,"is_masked":false}}"#).unwrap();
    let decoded = decode_query(envelope, false).unwrap();
    let id = cache_response(&mysql, "0", decoded, 10, &sink, &cancel)
        .await
        .unwrap()["results"][0]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let page = store.result_page(id, 0, 1, 16384).await.unwrap();
    assert_eq!(
        page.rows[0],
        vec![
            Value::Integer("9007199254740993".into()),
            Value::Decimal("12345678901234567890.123456789".into()),
            Value::Json(serde_json::from_str("9876543210123456789").unwrap())
        ]
    );
    let next = page.next.expect("second page");
    let page = store.result_page(id, next, 1, 16384).await.unwrap();
    assert_eq!(
        page.rows[0],
        vec![
            Value::Integer("2".into()),
            Value::Decimal("0.000000000000000001".into()),
            Value::Null
        ]
    );
    assert!(page.next.is_none());
    let mut masked = decode_query(
        response(json!(["secret"]), json!(["LONG"]), json!([["123"]])),
        false,
    )
    .unwrap();
    masked.is_masked = Some(true);
    let id = cache_response(&mysql, "0", masked, 10, &sink, &cancel)
        .await
        .unwrap()["results"][0]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        store.result_page(id, 0, 1, 16384).await.unwrap().rows[0],
        vec![Value::Text("123".into())]
    );
    assert_eq!(
        store.result_meta(id).await.unwrap().summary["masking"],
        "masked"
    );

    let redis = cache_session(ProviderKind::Redis);
    for rows in [
        json!([[0], [""], [null]]),
        json!([["0", []]]),
        json!([["4", []]]),
        json!([["0", ["first"]]]),
    ] {
        let columns = if rows[0].as_array().unwrap().len() == 1 {
            json!(["value"])
        } else {
            json!(["cursor", "keys"])
        };
        let decoded = decode_query(response(columns, json!([]), rows.clone()), true).unwrap();
        let id = cache_response(&redis, "0", decoded, 1, &sink, &cancel)
            .await
            .unwrap()["results"][0]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let meta = store.result_meta(id).await.unwrap();
        assert_eq!(meta.summary["coverage"]["original_query"], "unknown");
        let page = store.result_page(id, 0, 10, 16384).await.unwrap();
        if rows[0].as_array().unwrap().len() == 1 {
            assert_eq!(
                page.rows,
                vec![
                    vec![Value::Text("0".into())],
                    vec![Value::Text("".into())],
                    vec![Value::Null]
                ]
            );
        } else {
            assert_eq!(
                page.rows[0][0],
                Value::Text(rows[0][0].as_str().unwrap().into())
            );
            assert_eq!(page.rows[0][1], Value::Json(rows[0][1].clone()));
        }
    }
    drop(sink);
    store.pool.close().await;
    drop(store);
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[test]
fn query_preserves_order_duplicates_and_nulls() {
    let result = decode_query(
        response(
            json!(["x", "x"]),
            json!(["LONG", "VARCHAR"]),
            json!([[0, ""], [null, "a"]]),
        ),
        false,
    )
    .unwrap();
    assert_eq!(result.columns, ["x", "x"]);
    assert_eq!(result.rows[0], vec![json!(0), json!("")]);
    assert_eq!(result.rows[1][0], Json::Null);
    assert_eq!(result.affected_rows, Some(2));
    assert_eq!(result.mask_rule_hit, Some(false));
}

#[test]
fn query_rejects_invalid_shapes_and_preserves_redis_nested_values() {
    assert_eq!(
        decode_query(
            response(json!(["a"]), json!(["LONG"]), json!([[1, 2]])),
            false
        )
        .err()
        .unwrap()
        .code,
        "remote_protocol"
    );
    assert_eq!(
        decode_query(response(json!(["a"]), json!([]), json!([[1]])), false)
            .err()
            .unwrap()
            .code,
        "remote_protocol"
    );
    let redis = decode_query(
        response(json!(["Result"]), json!([]), json!([[["a", 1]]])),
        true,
    )
    .unwrap();
    assert_eq!(
        cell(redis.rows[0][0].clone(), "", true, false),
        Value::Json(json!(["a", 1]))
    );
    assert_eq!(
        cell(json!("123"), "LONG", false, true),
        Value::Text("123".into())
    );
}

#[test]
fn legacy_query_without_column_types_preserves_unknown_values() {
    let envelope = serde_json::from_value(json!({"status":0,"data":{
        "column_list":["big","empty","null"],"rows":[[9007199254740993u64,"",null]],"affected_rows":1
    }})).unwrap();
    let response = decode_query(envelope, false).unwrap();
    assert_eq!(response.columns, ["big", "empty", "null"]);
    assert!(response.types.is_empty());
    assert_eq!(
        cell(response.rows[0][0].clone(), "", false, false),
        Value::Json(json!(9007199254740993u64))
    );
    assert_eq!(
        cell(response.rows[0][1].clone(), "", false, false),
        Value::Text(String::new())
    );
    assert_eq!(
        cell(response.rows[0][2].clone(), "", false, false),
        Value::Null
    );
}

#[test]
fn redis_is_exact_and_serializes_posix_arguments() {
    let commands = redis_commands("get 'a b'\nGET \"\"\nGET \"a'b\"\nSCAN 0 COUNT 10").unwrap();
    assert_eq!(
        commands,
        [
            "GET 'a b'",
            "GET ''",
            "GET 'a'\\''b'",
            "SCAN '0' 'COUNT' '10'"
        ]
    );
    let input = "MGET \"\" \"a b\" \"a'b\" \"back\\\\slash\" \"line\\nnext\" \"中\"";
    let encoded = redis_commands(input).unwrap();
    assert_eq!(
        posix_words(&encoded[0]),
        vec!["MGET", "", "a b", "a'b", "back\\slash", "line\nnext", "中"]
    );
    for input in [
        "GETDEL x",
        "GETSET x y",
        "SUNIONSTORE x y",
        "ZRANGESTORE a b 0 1",
        "HSCAN x 0",
        "SELECT 1",
        "GET \\x00",
        "GET \\xff",
    ] {
        assert!(redis_commands(input).is_err(), "{input}");
    }
}

fn check(checked: Json, rows: Json) -> CheckDto {
    serde_json::from_value(json!({"checked":checked,"warning":null,"error":null,"warning_count":0,"error_count":0,"rows":rows,"status":null})).unwrap()
}

#[test]
fn sql_check_requires_completed_string_and_reports_verdict() {
    assert_eq!(
        decode_check(check(json!("True"), json!([{"errlevel":2}])))
            .unwrap()
            .verdict,
        "error"
    );
    assert_eq!(
        decode_check(check(json!("True"), json!([{"errlevel":1}])))
            .unwrap()
            .verdict,
        "warning"
    );
    assert_eq!(
        decode_check(check(json!("True"), json!([])))
            .unwrap()
            .verdict,
        "pass"
    );
    assert_eq!(
        decode_check(check(json!("False"), json!([])))
            .err()
            .unwrap()
            .code,
        "check_incomplete"
    );
    assert_eq!(
        decode_check(check(Json::Null, json!([])))
            .err()
            .unwrap()
            .code,
        "check_incomplete"
    );
    assert_eq!(
        decode_check(check(json!(true), json!([])))
            .err()
            .unwrap()
            .code,
        "remote_protocol"
    );
    assert_eq!(
        decode_check(check(json!("True"), json!([{}])))
            .err()
            .unwrap()
            .code,
        "remote_protocol"
    );
}

#[test]
fn preview_respects_utf8_boundary() {
    assert_eq!(preview("ab中", 4), ("ab", true));
}
