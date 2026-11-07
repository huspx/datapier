use super::*;

fn resources(keys: &[&[u8]]) -> Vec<Resource> {
    keys.iter()
        .map(|key| Resource {
            name: display(key),
            kind: "string".into(),
            database: None,
            key_b64: Some(encoded(key)),
            data: serde_json::Value::Null,
        })
        .collect()
}

fn keys(rows: &[Row<'_>]) -> Vec<usize> {
    rows.iter()
        .filter_map(|row| match row.kind {
            Kind::Key { index } => Some(index),
            _ => None,
        })
        .collect()
}

#[test]
fn default_collapse_and_nested_expansion_preserve_binary_and_empty_segments() {
    let profile = Uuid::new_v4();
    let owner = Some((profile, 0));
    let resources = resources(&[
        b"a",
        b"a:b:c",
        b"a::b",
        b":a:",
        b"a:\xff",
        b"base64:abc",
        b"\xff",
    ]);
    let tree = Tree::from_resources(&resources);
    let mut expansion = Expansion::default();
    assert_eq!(keys(&tree.rows(&expansion, owner)), [0, 6]);
    expansion.toggle(profile, 0, &encoded(b"a:"));
    assert_eq!(keys(&tree.rows(&expansion, owner)), [0, 4, 6]);
    for prefix in [b"a:b:".as_slice(), b"a::", b":", b":a:", b"base64:"] {
        expansion.toggle(profile, 0, &encoded(prefix));
    }
    let mut opened = keys(&tree.rows(&expansion, owner));
    opened.sort_unstable();
    assert_eq!(opened, [0, 1, 2, 3, 4, 5, 6]);
    expansion.toggle(profile, 0, &encoded(b"a:"));
    assert!(!keys(&tree.rows(&expansion, owner)).contains(&1));
    assert!(expansion.contains(profile, 0, b"a:b:"));
    assert_eq!(keys(&tree.rows(&expansion, Some((profile, 1)))), [0, 6]);
    assert_eq!(
        keys(&tree.rows(&expansion, Some((Uuid::new_v4(), 0)))),
        [0, 6]
    );
    assert_eq!(display(b"base64:abc"), "base64:YmFzZTY0OmFiYw==");
}

#[test]
fn leaf_labels_keep_only_the_last_segment_and_distinct_identities() {
    let profile = Uuid::new_v4();
    let resources = resources(&[
        b"prefix:a:same",
        b"other:same",
        b"prefix:a:",
        b"prefix:\xff",
    ]);
    let tree = Tree::from_resources(&resources);
    let mut expansion = Expansion::default();
    for prefix in [b"prefix:".as_slice(), b"prefix:a:", b"other:"] {
        expansion.toggle(profile, 0, &encoded(prefix));
    }
    let rows = tree.rows(&expansion, Some((profile, 0)));
    let mut leaves: Vec<_> = rows
        .iter()
        .filter_map(|row| match row.kind {
            Kind::Key { index } => Some((index, row.label)),
            _ => None,
        })
        .collect();
    leaves.sort_unstable_by_key(|(index, _)| *index);
    assert_eq!(
        leaves,
        vec![
            (0, b"same".as_slice()),
            (1, b"same"),
            (2, b""),
            (3, b"\xff")
        ]
    );
}

#[test]
fn deeply_delimited_key_stays_folded_without_recursive_storage() {
    let mut bytes = vec![b':'; 400_000];
    bytes.push(b'x');
    let resources = resources(&[&bytes]);
    let tree = Tree::from_resources(&resources);
    let rows = tree.rows(&Expansion::default(), None);
    assert!(matches!(
        &rows[..],
        [Row {
            kind: Kind::Directory {
                prefix: b":",
                expanded: false
            },
            depth: 0,
            ..
        }]
    ));
}

#[test]
fn later_pages_keep_existing_order_and_group_new_children() {
    let profile = Uuid::new_v4();
    let owner = Some((profile, 0));
    let mut expansion = Expansion::default();
    expansion.toggle(profile, 0, &encoded(b"b:"));
    let mut tree = Tree::from_resources(&resources(&[b"b:one", b"z", b"b:two"]));
    tree.append(&resources(&[b"a:new", b"b:three", b"a", b"\xff"]));
    let rows = tree.rows(&expansion, owner);
    let visible: Vec<_> = rows
        .iter()
        .map(|row| match row.kind {
            Kind::Directory { prefix, .. } => (prefix.to_vec(), None),
            Kind::Key { index } => (row.label.to_vec(), Some(index)),
        })
        .collect();
    assert_eq!(
        visible,
        vec![
            (b"b:".to_vec(), None),
            (b"one".to_vec(), Some(0)),
            (b"two".to_vec(), Some(2)),
            (b"three".to_vec(), Some(4)),
            (b"z".to_vec(), Some(1)),
            (b"a:".to_vec(), None),
            (b"a".to_vec(), Some(5)),
            (b"\xff".to_vec(), Some(6)),
        ]
    );
    expansion.toggle(profile, 0, &encoded(b"a:"));
    assert_eq!(keys(&tree.rows(&expansion, owner)), [0, 2, 4, 1, 3, 5, 6]);
}

#[test]
fn reset_replaces_keys_and_expansion_is_scoped_to_database() {
    let profile = Uuid::new_v4();
    let mut expansion = Expansion::default();
    expansion.toggle(profile, 0, &encoded(b"x:"));
    let mut tree = Tree::from_resources(&resources(&[b"x:old", b"old"]));
    tree.reset(&resources(&[b"x:\xff", b"new"]));
    let opened = tree.rows(&expansion, Some((profile, 0)));
    assert_eq!(keys(&opened), [0, 1]);
    assert_eq!(opened[1].label, b"\xff");
    let other_db = tree.rows(&expansion, Some((profile, 1)));
    assert_eq!(keys(&other_db), [1]);
    assert!(matches!(
        other_db[0].kind,
        Kind::Directory {
            expanded: false,
            ..
        }
    ));
    tree.reset(&[]);
    assert!(tree.rows(&expansion, Some((profile, 0))).is_empty());
}
