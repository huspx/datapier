//! 原生工作台英文词条，按界面职责维护。每个源词条只登记一次。
mod common;
mod connections;
mod projects;
mod schema;
mod settings;
mod transfers;
mod workbench;

pub(super) const CATALOG: &[&[(&str, &str)]] = &[
    common::ENTRIES,
    connections::ENTRIES,
    projects::ENTRIES,
    schema::ENTRIES,
    settings::ENTRIES,
    transfers::ENTRIES,
    workbench::ENTRIES,
];
