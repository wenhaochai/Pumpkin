//! The checks of the figures in `docs/figures/` and of the images that the repository's Markdown
//! documents embed.

use std::collections::BTreeSet;
use std::path::Path;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

use super::{split, word};
use crate::Result;
use crate::repo::Repo;

/// Where the figures that documents embed are, from the repository's root.
const FIGURES: &str = "docs/figures/";

/// What a figure must not hold, as it is text and shapes alone, in the system's fonts.
const NOT_IN_FIGURES: [&str; 3] = ["<image", "<script", "@font-face"];

/// Each problem, among `files`, the repository's, with an image that a Markdown document embeds:
/// one without alt text, or that isn't an SVG in [`FIGURES`] that is there; and with each file
/// in [`FIGURES`]: one that isn't an SVG, or that no document embeds; a figure without a
/// `<title>` or a `<desc>`, or that holds what [`NOT_IN_FIGURES`] lists; and a word that a figure
/// sets in its class `code`, split at `::`, that isn't among `names`.
pub(super) fn check(
    repo: &Repo<'_>,
    files: &[String],
    names: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let embedded = embeds(repo, files, &mut problems)?;
    for path in files.iter().filter(|path| path.starts_with(FIGURES)) {
        if !svg(path) {
            problems.push(format!(
                "{path}: a file in `{FIGURES}` that isn't an SVG: draw it as one, whose text \
                 stays text"
            ));
            continue;
        }
        if !embedded.contains(path) {
            problems.push(format!(
                "{path}: no document embeds the figure: embed it beside the prose that states \
                 what it shows, or remove it"
            ));
        }
        problems.extend(figure(path, &repo.read(path)?, names));
    }
    Ok(problems)
}

/// Whether `path` names an SVG.
fn svg(path: &str) -> bool {
    Path::new(path).extension() == Some("svg".as_ref())
}

/// The figures that `files`' Markdown documents embed, adding to `problems` each image that has
/// no alt text, or isn't an SVG in [`FIGURES`] among `files`.
fn embeds(
    repo: &Repo<'_>,
    files: &[String],
    problems: &mut Vec<String>,
) -> Result<BTreeSet<String>> {
    let mut embedded = BTreeSet::new();
    let documents = files
        .iter()
        .filter(|path| Path::new(path).extension() == Some("md".as_ref()));
    for document in documents {
        let text = repo.read(document)?;
        for (index, line) in text.lines().enumerate() {
            for (column, alt, target) in images(line) {
                let at = format!("{document}:{}:{column}: an image of `{target}`", index + 1);
                let figure = resolved(document, target);
                let is_figure = figure.as_deref().is_some_and(|figure| {
                    let name = figure.strip_prefix(FIGURES).unwrap_or_default();
                    !name.is_empty() && !name.contains('/') && svg(figure)
                });
                if !is_figure || target.contains("://") {
                    problems.push(format!(
                        "{at}, which isn't an SVG in `{FIGURES}`: draw it as one there, and \
                         embed that"
                    ));
                } else if let Some(figure) = figure.filter(|figure| files.contains(figure)) {
                    embedded.insert(figure);
                    if alt.trim().is_empty() {
                        problems.push(format!(
                            "{at} has no alt text: say in it what the figure shows, for a \
                             reader who can't see it"
                        ));
                    }
                } else {
                    problems.push(format!(
                        "{at}, which isn't in the repository: embed a figure that is, or remove \
                         the image"
                    ));
                }
            }
        }
    }
    Ok(embedded)
}

/// Each problem with `text`, the figure at `path`: no `<title>` or `<desc>`, what
/// [`NOT_IN_FIGURES`] lists, or a word it sets as code that isn't among `names`.
fn figure(path: &str, text: &str, names: &BTreeSet<String>) -> Vec<String> {
    let mut problems = Vec::new();
    for (element, what) in [
        ("<title", "names what it shows"),
        ("<desc", "says it in prose"),
    ] {
        if !text.contains(&format!("{element}>")) && !text.contains(&format!("{element} ")) {
            problems.push(format!(
                "{path}: the figure has no `{element}>`: add one that {what}, which a \
                     screen reader reads"
            ));
        }
    }
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        for banned in NOT_IN_FIGURES
            .iter()
            .filter(|banned| line.contains(*banned))
        {
            problems.push(format!(
                "{path}:{line_number}: the figure holds `{banned}`: remove it, as a figure \
                     is text and shapes alone, in the system's fonts"
            ));
        }
        for code in coded(line) {
            for word in code.split_whitespace().flat_map(|word| word.split("::")) {
                if !names.contains(word) {
                    problems.push(format!(
                        "{path}:{line_number}: the figure sets `{word}` as code, but nothing \
                             in the workspace is named so: name it as the code does now, or set \
                             it as prose"
                    ));
                }
            }
        }
    }
    problems
}

/// Each image that `line` of Markdown embeds, as a Markdown image or as an HTML `<img>` whose tag
/// the line holds whole, in the order they start: the column its `!` or `<` is at, counted from
/// 1, its alt text and its target. An `<img>` without an `alt` or a `src` has an empty one.
fn images(line: &str) -> Vec<(usize, &str, &str)> {
    let column = |at: usize| line[..at].chars().count() + 1;
    let mut images = Vec::new();
    for (at, _) in line.match_indices("![") {
        let rest = &line[at + 2..];
        let Some(close) = rest.find(']') else {
            continue;
        };
        let Some(target) = rest[close + 1..].strip_prefix('(') else {
            continue;
        };
        let target = &target[..target.find(')').unwrap_or(target.len())];
        images.push((column(at), &rest[..close], target));
    }
    for (at, _) in line.match_indices("<img") {
        let rest = &line[at + "<img".len()..];
        let Some(close) = rest.find('>') else {
            continue;
        };
        let tag = &rest[..close];
        if !tag.starts_with(char::is_whitespace) {
            continue;
        }
        let alt = attribute(tag, "alt").unwrap_or_default();
        images.push((column(at), alt, attribute(tag, "src").unwrap_or_default()));
    }
    images.sort_by_key(|(column, _, _)| *column);
    images
}

/// The value of the attribute `name` that `tag`, the inside of an HTML tag after its name, gives
/// in double quotes.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let opening = format!("{name}=\"");
    tag.match_indices(&opening).find_map(|(at, _)| {
        if !tag[..at].ends_with(char::is_whitespace) {
            return None;
        }
        let value = &tag[at + opening.len()..];
        value.find('"').map(|end| &value[..end])
    })
}

/// The path from the repository's root of `target`, which `document` names relative to its own
/// directory, if it stays below the root.
fn resolved(document: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = document.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// The text of each element that `line` of a figure opens with the class `code`, up to the next
/// tag.
fn coded(line: &str) -> Vec<&str> {
    let mut coded = Vec::new();
    for (at, opening) in line.match_indices("class=\"") {
        let value = &line[at + opening.len()..];
        let value = &value[..value.find('"').unwrap_or(value.len())];
        if value.split_whitespace().any(|class| class == "code") {
            let after = &line[at..];
            if let Some(start) = after.find('>') {
                let text = &after[start + 1..];
                coded.push(&text[..text.find('<').unwrap_or(text.len())]);
            }
        }
    }
    coded
}

/// The names of the packages and binaries that `files`' `Cargo.toml` manifests declare.
pub(super) fn packages(repo: &Repo<'_>, files: &[String]) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for path in files
        .iter()
        .filter(|path| path.rsplit('/').next() == Some("Cargo.toml"))
    {
        let manifest: toml::Table = toml::from_str(&repo.read(path)?)
            .map_err(|error| format!("{path} isn't a manifest cargo reads: {error}"))?;
        let package = manifest
            .get("package")
            .and_then(|package| package.get("name"));
        let bins = manifest.get("bin").and_then(toml::Value::as_array);
        let bins = bins.into_iter().flatten().filter_map(|bin| bin.get("name"));
        for name in package
            .into_iter()
            .chain(bins)
            .filter_map(toml::Value::as_str)
        {
            names.insert(name.to_owned());
        }
    }
    Ok(names)
}

/// Adds to `names` each name that `tokens`, and the groups in them, declare: of an item, a
/// module, an enum's variant, and a variant's as a command line spells it, `install-tools` for
/// `InstallTools`; and each string literal's text, such as a file's name.
pub(super) fn defined(tokens: TokenStream, names: &mut BTreeSet<String>) {
    const ITEMS: [&str; 9] = [
        "fn", "struct", "enum", "trait", "type", "mod", "const", "static", "union",
    ];
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    for (at, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Ident(keyword) if ITEMS.contains(&word(token).as_str()) => {
                let Some(TokenTree::Ident(name)) = tokens.get(at + 1) else {
                    continue;
                };
                names.insert(name.to_string());
                if keyword == "enum" {
                    let body = tokens[at..].iter().find_map(|token| match token {
                        TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
                            Some(group)
                        }
                        _ => None,
                    });
                    for variant in body.into_iter().flat_map(variants) {
                        names.insert(split(&variant).join("-"));
                        names.insert(variant);
                    }
                }
            }
            TokenTree::Literal(literal) => {
                let literal = literal.to_string();
                if let Some(text) = literal
                    .strip_prefix('"')
                    .and_then(|text| text.strip_suffix('"'))
                {
                    names.insert(text.to_owned());
                }
            }
            TokenTree::Group(group) => defined(group.stream(), names),
            TokenTree::Ident(_) | TokenTree::Punct(_) => {}
        }
    }
}

/// The names of the variants that `body`, an enum's, declares.
fn variants(body: &proc_macro2::Group) -> Vec<String> {
    let mut variants = Vec::new();
    let mut first = true;
    let mut attribute = false;
    for token in body.stream() {
        match &token {
            TokenTree::Punct(punct) if punct.as_char() == ',' => first = true,
            TokenTree::Punct(punct) if punct.as_char() == '#' => attribute = true,
            TokenTree::Group(_) if attribute => attribute = false,
            TokenTree::Ident(name) if first => {
                variants.push(name.to_string());
                first = false;
            }
            _ => first = false,
        }
    }
    variants
}
