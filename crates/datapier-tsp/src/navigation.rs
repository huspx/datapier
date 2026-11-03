use super::*;
use crate::i18n::UiMessage;
use std::collections::{BTreeMap, BTreeSet};

mod action;
mod drop;
mod profile;
mod response;
mod state;
mod tree;

pub(super) use state::{
    DatabaseForm, NavigationState, ProfileDeleteConfirmation, ResourceDropConfirmation,
    navigation_visible_databases,
};
use state::{DropOperation, Pending, ProfileDeleteOperation, QueuedSection};

#[cfg(test)]
#[path = "../tests/unit/navigation.rs"]
mod tests;
