use super::table_options;
use sqlparser::ast::{CreateTableOptions, Expr, Ident, ObjectName, SqlOption};

#[test]
fn qualified_option_keys_are_preserved() {
    let option = |parts: Vec<Ident>| SqlOption::KeyValue {
        key: ObjectName::from(parts),
        value: Expr::Identifier(Ident::new("InnoDB")),
    };
    let options = CreateTableOptions::Plain(vec![
        option(vec![Ident::new("ENGINE")]),
        option(vec![Ident::new("other"), Ident::new("ENGINE")]),
    ]);
    assert_eq!(
        table_options(options).unwrap(),
        vec!["other.ENGINE = InnoDB"]
    );
}
