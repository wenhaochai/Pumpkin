//! A policy as IAM's JSON, which Terraform's `templatefile` fills: a placeholder as `${name}`,
//! and a list as `${jsonencode(name)}`, in the place of a condition's whole array of values:
//! <https://developer.hashicorp.com/terraform/language/functions/templatefile>.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use super::{
    ACCOUNT, Allow, Condition, Key, List, PARTITION, Part, Placeholder, REGION, Resource, Values,
};

/// A placeholder of a policy, as [`placeholders`] gives it: what it stands for, and whether for a
/// list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    pub about: &'static str,
    pub list: bool,
}

/// The policy that allows each statement of `allows`, as IAM's JSON, with each placeholder as
/// `templatefile` takes it.
///
/// Statements of the same resources and conditions are one, with each of their actions, ordered
/// by those resources and conditions.
// As `controller-gen rbac` merges the rules of the same resources, and iamlive the statements of
// the same resources, whatever their conditions, as its statements have none:
// https://github.com/kubernetes-sigs/controller-tools/blob/9d91b2512c200fb64555b0e5b5c4d4ee76894ef3/pkg/rbac/parser.go#L184-L213
// https://github.com/iann0036/iamlive/blob/d8e67a2d9a422a00c9246531b3a5f3892c465585/iamlivecore/logger.go#L149-L167
pub fn policy(allows: &[&'static Allow]) -> String {
    // By their resources and the text of their conditions, which a `Map` isn't ordered by.
    let mut merged: BTreeMap<(BTreeSet<String>, String), Merged> = BTreeMap::new();
    for allow in allows {
        let Allow {
            actions,
            resources,
            conditions,
        } = allow;
        let resources = resources.iter().map(resource).collect();
        let conditions = tested(conditions);
        let text = Value::Object(conditions.clone()).to_string();
        let (_, merged) = merged
            .entry((resources, text))
            .or_insert((conditions, BTreeSet::new()));
        merged.extend(actions.iter().map(|action| literal(action)));
    }
    let statements = merged
        .into_iter()
        .map(|((resources, _), (conditions, actions))| {
            let mut statement = Map::new();
            statement.insert("Effect".to_owned(), "Allow".into());
            statement.insert("Action".to_owned(), json!(actions));
            statement.insert("Resource".to_owned(), json!(resources));
            if !conditions.is_empty() {
                statement.insert("Condition".to_owned(), Value::Object(conditions));
            }
            Value::Object(statement)
        });
    let policy = json!({ "Version": "2012-10-17", "Statement": statements.collect::<Vec<_>>() });
    let mut text = format!("{policy:#}\n");
    // A list stands for the whole array of a condition's values, so it goes unquoted.
    for (name, fill) in placeholders(allows) {
        if fill.list {
            let bare = listed_text(name);
            text = text.replace(&format!("\"{bare}\""), &bare);
        }
    }
    text
}

/// The placeholders of `allows`, each once, by its name.
///
/// # Panics
///
/// If two placeholders of the same name differ, in what they stand for or in whether for a list.
pub fn placeholders(allows: &[&Allow]) -> BTreeMap<&'static str, Fill> {
    let mut found = BTreeMap::new();
    let mut fill = |name: &'static str, about: &'static str, list: bool| {
        let fill = Fill { about, list };
        let held = *found.entry(name).or_insert(fill);
        assert!(
            held == fill,
            "two placeholders are named `{name}`: name each its own"
        );
    };
    for allow in allows {
        for resource in allow.resources {
            match resource {
                Resource::Any => {}
                Resource::Arn(placeholder) | Resource::Below(placeholder, _) => {
                    fill(placeholder.name, placeholder.about, false);
                }
                Resource::In(_, path) => {
                    for placeholder in [&PARTITION, &REGION, &ACCOUNT]
                        .into_iter()
                        .chain(filled(path))
                    {
                        fill(placeholder.name, placeholder.about, false);
                    }
                }
            }
        }
        for condition in allow.conditions {
            match &condition.values {
                Values::Are(values) => {
                    for placeholder in filled(values) {
                        fill(placeholder.name, placeholder.about, false);
                    }
                }
                Values::Listed(List(placeholder)) => {
                    fill(placeholder.name, placeholder.about, true);
                }
            }
        }
    }
    found
}

/// A statement's conditions, and its actions.
type Merged = (Map<String, Value>, BTreeSet<String>);

fn filled(parts: &'static [Part]) -> impl Iterator<Item = &'static Placeholder> {
    parts.iter().filter_map(|part| match part {
        Part::Text(_) => None,
        Part::Fill(placeholder) => Some(*placeholder),
    })
}

/// `conditions` as a statement's `Condition` holds them: by operator, then by key.
fn tested(conditions: &[Condition]) -> Map<String, Value> {
    let mut tests: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for Condition { test, key, values } in conditions {
        let values = match values {
            Values::Are(parts) => json!(parts.iter().map(part_text).collect::<Vec<_>>()),
            Values::Listed(List(placeholder)) => Value::from(listed_text(placeholder.name)),
        };
        let keys = tests.entry(format!("{test:?}")).or_default();
        keys.insert(key_text(key), values);
    }
    let tests = tests
        .into_iter()
        .map(|(test, keys)| (test, Value::Object(keys)));
    tests.collect()
}

fn resource(resource: &Resource) -> String {
    match resource {
        Resource::Any => "*".to_owned(),
        Resource::Arn(placeholder) => fill_text(placeholder),
        Resource::Below(placeholder, path) => {
            format!("{}/{}", fill_text(placeholder), literal(path))
        }
        Resource::In(service, path) => {
            let path: String = path.iter().map(part_text).collect();
            let [partition, region, account] = [&PARTITION, &REGION, &ACCOUNT].map(fill_text);
            let service = literal(service);
            format!("arn:{partition}:{service}:{region}:{account}:{path}")
        }
    }
}

fn key_text(key: &Key) -> String {
    match key {
        Key::Named(name) => literal(name),
        Key::Tag(service, tag) => literal(&format!("{service}:ResourceTag/{tag}")),
    }
}

fn part_text(part: &Part) -> String {
    match part {
        Part::Text(text) => literal(text),
        Part::Fill(placeholder) => fill_text(placeholder),
    }
}

/// `text`, which `templatefile` gives back as it is: each `${` and `%{`, which would begin its
/// own interpolation or directive, escaped, so that an IAM policy variable such as
/// `${aws:username}` reaches IAM.
// https://developer.hashicorp.com/terraform/language/expressions/strings
fn literal(text: &str) -> String {
    text.replace("${", "$${").replace("%{", "%%{")
}

fn fill_text(placeholder: &Placeholder) -> String {
    format!("${{{}}}", placeholder.name)
}

fn listed_text(name: &str) -> String {
    format!("${{jsonencode({name})}}")
}
