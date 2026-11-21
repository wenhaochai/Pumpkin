//! Tests of xtask's modules, on scratch Git repositories where they use Git, and of the consumer
//! entry `deploy/pumpkin`. xtask is a binary, so its test binary compiles their modules itself.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::error::Error;

use expect_test::{Expect, expect};
use test_support::TestDir;
use xshell::{Shell, cmd};

#[path = "../../src/diff.rs"]
mod diff;
mod entry;
#[path = "../../src/figure.rs"]
mod figure;
#[path = "../../src/gate.rs"]
mod gate;
mod live;
mod op_stats;
mod release;
#[path = "../../src/settings.rs"]
mod settings;
#[path = "../../src/tidy.rs"]
mod tidy;
#[path = "../../src/wait.rs"]
mod wait;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// In the repository's one commit.
    Tracked,
    Untracked,
    /// In a directory the repository ignores.
    Ignored,
    /// Tracked, then deleted from the working tree.
    Deleted,
}

/// Writes `files`, each a path, its text and how it stands, into a new Git repository, and runs
/// `run` there; the repository's path shows as `<repo>`.
fn check(files: &[(&str, String, Standing)], run: impl FnOnce(&Shell) -> String, expect: &Expect) {
    let scratch = TestDir::new();
    let dir = scratch.path();
    let sh = Shell::new().unwrap();
    sh.change_dir(dir);
    // Git's own variables, inherited, would steer every command here to another repository, as
    // the `GIT_DIR` that `git rebase --exec` sets in a linked worktree would. A `Shell` can set
    // a variable but not remove one, so each that places a repository is set to this one's.
    let git = dir.join(".git");
    sh.set_var("GIT_DIR", &git);
    sh.set_var("GIT_COMMON_DIR", &git);
    sh.set_var("GIT_WORK_TREE", dir);
    sh.set_var("GIT_INDEX_FILE", git.join("index"));
    sh.set_var("GIT_OBJECT_DIRECTORY", git.join("objects"));
    // Nor does the user's configuration, which could change what a diff prints.
    sh.set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    sh.set_var("GIT_CONFIG_NOSYSTEM", "1");
    for name in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
        sh.set_var(name, "xtask");
    }
    for email in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
        sh.set_var(email, "xtask@example.com");
    }
    cmd!(sh, "git init --quiet").quiet().run().unwrap();
    sh.write_file(".gitignore", "ignored/\n").unwrap();
    for (path, text, standing) in files {
        sh.write_file(path, text).unwrap();
        if matches!(standing, Standing::Tracked | Standing::Deleted) {
            cmd!(sh, "git add -- {path}").quiet().run().unwrap();
        }
        if *standing == Standing::Deleted {
            sh.remove_path(path).unwrap();
        }
    }
    cmd!(sh, "git commit --quiet --allow-empty --message base")
        .quiet()
        .run()
        .unwrap();
    let reported = run(&sh);
    expect.assert_eq(&reported.replace(dir.to_str().unwrap(), "<repo>"));
}

fn tidy(sh: &Shell) -> String {
    match tidy::check(sh) {
        Ok(()) => "passed".to_owned(),
        Err(error) => error.to_string(),
    }
}

fn lines(line: &str, count: usize) -> String {
    format!("{line}\n").repeat(count)
}

#[test]
fn caps() {
    use Standing::{Deleted, Ignored, Tracked, Untracked};
    let at_caps = lines("//! A line.", 25) + &lines("/// A line.", 9) + "fn f() {}\n";
    let behind_attribute = format!(
        "#![expect(\n    dead_code,\n    reason = \"a reason\"\n)]\n{}",
        lines("//! A line.", 26)
    );
    let in_parts = format!(
        "/* A comment. */\n{}\n#![doc = \"A line.\"]\n{}",
        lines("//! A line.", 15),
        lines("//! A line.", 10)
    );
    let item_in_parts = format!(
        "{}#[derive(Debug)]\n\n// A comment.\n{}#[doc = \"A line.\"]\nstruct S;\n",
        lines("/// A line.", 5),
        lines("/// A line.", 4)
    );
    let raw = format!(
        "const S: &str = r#\"\n{}\"#;\nconst B: &[u8] = br\"\n{}\";\nfn f() {{}}\n",
        lines("/// Not a doc.", 20),
        lines("//! Not a doc.", 41)
    );
    let separate =
        lines("/// A line.", 9) + "fn a() {}\n" + &lines("/// A line.", 9) + "fn b() {}\n";
    let long_item = lines("/// A line.", 10) + "fn f() {}\n";
    let method = format!(
        "struct S;\nimpl S {{\n{}    fn f() {{}}\n}}\n",
        lines("    /// A line.", 10)
    );
    let inline_module = format!("mod m {{\n{}}}\n", lines("    //! A line.", 26));
    let macro_rule = format!(
        "macro_rules! m {{\n    () => {{\n{}        fn f() {{}}\n    }};\n}}\n",
        lines("        /// A line.", 10)
    );
    check(
        &[
            ("at_caps.rs", at_caps, Tracked),
            ("behind_attribute.rs", behind_attribute, Tracked),
            ("in_parts.rs", in_parts, Tracked),
            ("item_in_parts.rs", item_in_parts, Tracked),
            ("raw.rs", raw, Tracked),
            ("separate.rs", separate, Tracked),
            ("method.rs", method, Tracked),
            ("inline_module.rs", inline_module, Tracked),
            ("macro_rule.rs", macro_rule, Tracked),
            ("é.rs", long_item.clone(), Tracked),
            ("untracked.rs", long_item.clone(), Untracked),
            ("ignored/long.rs", long_item.clone(), Ignored),
            ("deleted.rs", long_item, Deleted),
        ],
        tidy,
        &expect![[r#"
            untracked.rs:1: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            behind_attribute.rs:5: the module doc is 26 lines, over the cap of 25: shorten it to 25 or fewer, moving each fact to its home
            in_parts.rs:2: the module doc is 26 lines, over the cap of 25: shorten it to 25 or fewer, moving each fact to its home
            inline_module.rs:2: the module doc is 26 lines, over the cap of 25: shorten it to 25 or fewer, moving each fact to its home
            item_in_parts.rs:1: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            macro_rule.rs:3: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            method.rs:3: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            é.rs:1: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home"#]],
    );
}

#[test]
fn forms() {
    use Standing::Tracked;
    let example = format!(
        "/// ```\n/// let s = r#\"\n{}/// \"#;\n/// ```\nfn f() {{}}\n",
        lines("/// A line.", 6)
    );
    let strings = format!(
        "const A: &str = \"-r\";\nconst B: &str = \"r#\";\nconst C: &CStr = cr\"\n{}\";\n\
         const D: &str = \"\n{}\";\n{}fn f() {{}}\n",
        lines("/// Not a doc.", 16),
        lines("//! Not a doc.", 41),
        lines("/// A line.", 10)
    );
    let bracket = format!(
        "#[expect(\n    dead_code,\n    reason = \"a [bracket\"\n)]\nfn a() {{}}\n{}fn b() {{}}\n",
        lines("/// A line.", 10)
    );
    let blocks = format!(
        "/*!\n{}*/\n/**\n{}*/\nfn f() {{}}\n",
        lines(" A line.", 24),
        lines(" A line.", 8)
    );
    let attributes = format!(
        "#[doc = \"A line.\n{}\"]\nfn f() {{}}\n{}#[doc(hidden)]\nfn g() {{}}\n",
        lines("A line.", 8),
        lines("/// A line.", 9)
    );
    let invalid = "fn f() {\n    let c = 'ab';\n}\n".to_owned();
    check(
        &[
            ("example.rs", example, Tracked),
            ("strings.rs", strings, Tracked),
            ("bracket.rs", bracket, Tracked),
            ("blocks.rs", blocks, Tracked),
            ("attributes.rs", attributes, Tracked),
            ("invalid.rs", invalid, Tracked),
        ],
        tidy,
        &expect![[r#"
            attributes.rs:1: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            blocks.rs:1: the module doc is 26 lines, over the cap of 25: shorten it to 25 or fewer, moving each fact to its home
            blocks.rs:27: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            bracket.rs:6: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            example.rs:1: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            invalid.rs:2:13: the file is not valid Rust here, so it can't be checked: make it valid Rust
            strings.rs:64: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home"#]],
    );
}

#[test]
fn unreachable() {
    use Standing::Tracked;
    let bare = format!(
        "fn f() {{\n    unreachable!()\n}}\n{}fn g() {{\n    std::unreachable![];\n}}\n\
         macro_rules! m {{\n    () => {{ core::unreachable! {{}} }};\n}}\n\
         fn h(x: u8) {{\n    debug_assert!(x > 0);\n    std::debug_assert_eq!(x, 1, \"why\");\n    \
         debug_assert_ne![x, 2];\n}}\n",
        lines("/// A line.", 10)
    );
    let messages = "#![expect(unreachable_pub, reason = \"unreachable!()\")]\n\
                    fn f(x: u8) {\n    unreachable!(\"x is {x}\");\n    unreachable!(\"why\", );\n\
                    // unreachable!()\n    let _ = \"unreachable!()\";\n    unreachable(());\n}\n\
                    fn h(x: u8) {\n    assert!(x > 0);\n    assert_eq!(x, 1);\n    \
                    // debug_assert!(x)\n    let _ = \"debug_assert!(x)\";\n    debug_assert(x);\n}\n"
        .to_owned();
    check(
        &[
            ("bare.rs", bare, Tracked),
            ("messages.rs", messages, Tracked),
        ],
        tidy,
        &expect![[r#"
            bare.rs:2:5: `unreachable!` has no message: say in one why only a bug can reach it
            bare.rs:4: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            bare.rs:15:10: `unreachable!` has no message: say in one why only a bug can reach it
            bare.rs:18:19: `unreachable!` has no message: say in one why only a bug can reach it
            bare.rs:21:5: `debug_assert!` is skipped by release builds: use `assert!`, which guards the real run too
            bare.rs:22:10: `debug_assert_eq!` is skipped by release builds: use `assert_eq!`, which guards the real run too
            bare.rs:23:5: `debug_assert_ne!` is skipped by release builds: use `assert_ne!`, which guards the real run too"#]],
    );
}

#[test]
fn test_docs() {
    use Standing::Tracked;
    let tests = "/// A doc.\n#[test]\nfn before() {}\n\
                 #[test]\n/// A doc.\n#[ignore = \"a reason\"]\nfn between() {}\n\
                 mod m {\n    #[test]\n    /** A doc. */\n    fn inner() {}\n}\n\
                 #[doc = \"A doc.\"]\n#[test]\nfn attribute() {}\n\
                 #[test]\n// A comment.\nfn commented() {}\n\
                 /// A doc.\nfn helper() {}\n\
                 /// A doc.\n#[cfg(test)]\nfn configured() {}\n\
                 // One.\n// Two.\n#[test]\nfn two_above() {}\n\
                 // One.\n// Two.\n// Three.\n#[test]\nfn three_above() {}\n\
                 #[test]\n    // One.\n    // Two.\n// Three.\n\
                 #[ignore = \"a reason\"]\nfn three_among() {}\n\
                 // One.\n// Two.\n#[test]\n// Three.\nfn split() {}\n\
                 // One.\n// Two.\n// Three.\n\n#[test]\nfn apart() {}\n\
                 // One.\n// Two.\n// Three.\nfn helper_three() {}\n"
        .to_owned();
    check(
        &[("tests.rs", tests, Tracked)],
        tidy,
        &expect![[r#"
            tests.rs:1:1: a test has a doc: remove it, and put a why the test needs in a `//` comment at the line it explains
            tests.rs:5:1: a test has a doc: remove it, and put a why the test needs in a `//` comment at the line it explains
            tests.rs:10:5: a test has a doc: remove it, and put a why the test needs in a `//` comment at the line it explains
            tests.rs:13:1: a test has a doc: remove it, and put a why the test needs in a `//` comment at the line it explains
            tests.rs:28:1: a test has a `//` block of 3 lines, over the cap of 2: keep to a why of a line or two, at the line it explains
            tests.rs:34:5: a test has a `//` block of 3 lines, over the cap of 2: keep to a why of a line or two, at the line it explains"#]],
    );
}

#[test]
fn comments() {
    use Standing::Tracked;
    let markers = "//! A TODO.\n/// A FIXME, then an XXX.\nfn f() {\n    // TODO: one.\n    \
                   let _ = 1; /* a FIXME */\n}\n/** XXX */\n/* Nested /* TODO */ */\nfn g() {}\n\
                   // TODOS, XXXX, FIXME_ONE, A_TODO and todo are words of their own.\n"
        .to_owned();
    let code = "fn f(v: &[u8]) -> Option<u8> {\n    // f(v);\n    // let x = 1;\n    \
                // if v.is_empty() {\n    // }\n    // v[0];\n    //// f(v)?;\n    \
                // A clause;\n    // a `v[0]` and its end;\n    Some(v[0])\n}\n\
                // At the end, with no newline: f(v);"
        .to_owned();
    // `columns` columns: `start`, then words.
    let wide = |start: &str, columns: usize| {
        let words = "ab ".repeat(columns);
        format!("{start}{}x\n", &words[..columns - start.len() - 1])
    };
    let widths = [
        wide("// ", 100),
        wide("// ", 101),
        wide("    /// ", 101),
        wide("//! ", 120),
        wide("let _ = 1; // ", 101),
        "/* A block,\n".to_owned() + &wide(" * ", 101) + " */\n",
        format!("// | {} |\n", "a | ".repeat(30)),
        format!("// https://example.com/{}\n", "a".repeat(100)),
        format!("const S: &str = \"\n{}\";\n", wide("// ", 101)),
        "fn f() {}\n".to_owned(),
    ]
    .concat();
    let literals = "/// ```\n/// f(v);\n/// ```\n#[doc = \"A TODO.\"]\n\
                    fn f() {\n    /* f(v); */\n    let _ = \"// TODO: f(v);\";\n    \
                    let _ = r#\"\n// let x = 1;\n\"#;\n    let _ = '/';\n    let _ = 1 / 2;\n}\n"
        .to_owned();
    check(
        &[
            ("markers.rs", markers, Tracked),
            ("code.rs", code, Tracked),
            ("literals.rs", literals, Tracked),
            ("widths.rs", widths, Tracked),
        ],
        tidy,
        &expect![[r#"
            code.rs:2:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:3:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:4:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:5:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:6:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:7:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:12:1: a `//` comment holds code: remove it, as Git keeps what was there
            markers.rs:1:7: a comment holds `TODO`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:2:7: a comment holds `FIXME`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:2:22: a comment holds `XXX`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:4:8: a comment holds `TODO`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:5:21: a comment holds `FIXME`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:7:5: a comment holds `XXX`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:8:14: a comment holds `TODO`: open an issue for the work left, and say a known limit as a why at its code
            widths.rs:2:101: a line of a comment is 101 columns, over the cap of 100: wrap it
            widths.rs:3:101: a line of a comment is 101 columns, over the cap of 100: wrap it
            widths.rs:4:101: a line of a comment is 120 columns, over the cap of 100: wrap it
            widths.rs:5:101: a line of a comment is 101 columns, over the cap of 100: wrap it
            widths.rs:7:101: a line of a comment is 101 columns, over the cap of 100: wrap it"#]],
    );
}

#[test]
fn restated() {
    use Standing::Tracked;
    let docs = "/// A lease.\nstruct LeaseId;\n\
                /// A lease's id, which it keeps once closed.\nstruct LeaseKey;\n\
                /// The SHA-256 digest of `bytes`.\nfn digest(bytes: &[u8]) -> Sha256Digest {}\n\
                /// `/`.\nfn root() {}\n\
                /// ```\n/// A lease.\n/// ```\nstruct Lease;\n\
                /// A [lease](https://example.com/lease-id).\n#[derive(Debug)]\n\
                pub const fn lease_id() {}\n\
                struct S {\n    /// When it was opened.\n    pub(crate) opened: u64,\n    \
                /// When it closes, which no lease outlives.\n    closes: u64,\n}\n\
                enum E {\n    /// At this deadline.\n    Until(Deadline),\n}\n\
                #[doc = \"A lease.\"]\nstruct LeaseRow;\n"
        .to_owned();
    check(
        &[("docs.rs", docs, Tracked)],
        tidy,
        &expect![[r#"
            docs.rs:1:1: the doc's first sentence only restates `LeaseId`, its type or its parameters: say what they leave a caller room to get wrong, or remove the doc
            docs.rs:5:1: the doc's first sentence only restates `digest`, its type or its parameters: say what they leave a caller room to get wrong, or remove the doc
            docs.rs:13:1: the doc's first sentence only restates `lease_id`, its type or its parameters: say what they leave a caller room to get wrong, or remove the doc
            docs.rs:17:5: the doc's first sentence only restates `opened`, its type or its parameters: say what they leave a caller room to get wrong, or remove the doc
            docs.rs:23:5: the doc's first sentence only restates `Until`, its type or its parameters: say what they leave a caller room to get wrong, or remove the doc"#]],
    );
}

#[test]
fn architecture() {
    use Standing::Tracked;
    let map = "# Architecture\n\nThe rules are in [`AGENTS.md`](AGENTS.md), as in\n\
               [rust-analyzer's](https://github.com/rust-lang/rust-analyzer), under\n\
               [Crates](#crates) and [`style`](docs/dev/style.md#errors), with\n\
               [`main.rs`](src/main.rs) in [`src`](src/).\n"
        .to_owned();
    check(
        &[
            ("ARCHITECTURE.md", map, Tracked),
            ("AGENTS.md", "# Agents\n".to_owned(), Tracked),
            ("src/main.rs", "fn main() {}\n".to_owned(), Tracked),
        ],
        tidy,
        &expect![[r#"
            ARCHITECTURE.md:5:33: a link to `docs/dev/style.md#errors`, which isn't in the repository: link a document that is, or remove the link
            ARCHITECTURE.md:6:13: a link to `src/main.rs`, code in the repository: name it instead, in backticks
            ARCHITECTURE.md:6:37: a link to `src/`, code in the repository: name it instead, in backticks"#]],
    );
}

#[test]
fn figures() {
    use Standing::Tracked;
    let figure =
        |title: &str, body: &str| format!("<svg>\n{title}<desc>A figure.</desc>\n{body}</svg>\n");
    let good = figure(
        "<title>A figure</title>",
        "<text class=\"code mid\">a-crate</text><text class=\"code\">a-bin</text>\n\
         <text class=\"note\">Words, not code. <tspan class=\"code\">run</tspan></text>\n\
         <text class=\"name code\">Run::prepare install-tools outcome.json</text>\n",
    );
    let bad = "<svg>\n<style>@font-face{}</style>\n<script>run()</script>\n\
               <image href=\"x.png\"/><text class=\"code\">Gone::prepare</text>\n</svg>\n"
        .to_owned();
    let map = "# Architecture\n\n![The machines.](docs/figures/good.svg) beside\n\
               ![](docs/figures/bad.svg), ![A chart.](docs/chart.png) and\n\
               ![A gone one.](docs/figures/gone.svg), with `#![no_std]` and `expect![[\"\"]]`.\n"
        .to_owned();
    let guide = "![Again.](../figures/good.svg) and ![Out.](../../../good.svg) and \
                 ![Far.](https://example.com/a.svg)\n\
                 <p align=\"center\"><img alt=\"A mark.\" width=\"160\" src=\"../figures/mark.svg\"></p> \
                 <img src=\"../figures/good.svg\"> <img src=\"../chart.png\" alt=\"A chart.\"> <imgs>\n"
        .to_owned();
    let code = "enum Command {\n    InstallTools,\n    #[expect(dead_code)]\n    Run(Run),\n}\n\
                struct Run;\nfn prepare() {}\nconst OUTCOME: &str = \"outcome.json\";\n"
        .to_owned();
    let manifest = "[package]\nname = \"a-crate\"\n\n[[bin]]\nname = \"a-bin\"\n".to_owned();
    check(
        &[
            ("ARCHITECTURE.md", map, Tracked),
            ("docs/dev/guide.md", guide, Tracked),
            ("docs/figures/good.svg", good, Tracked),
            ("docs/figures/bad.svg", bad, Tracked),
            (
                "docs/figures/mark.svg",
                figure("<title>A mark</title>", ""),
                Tracked,
            ),
            (
                "docs/figures/orphan.svg",
                figure("<title id=\"t\">An orphan</title>", ""),
                Tracked,
            ),
            ("docs/figures/chart.png", "PNG\n".to_owned(), Tracked),
            ("Cargo.toml", manifest, Tracked),
            ("src/lib.rs", code, Tracked),
        ],
        tidy,
        &expect![[r#"
            ARCHITECTURE.md:4:1: an image of `docs/figures/bad.svg` has no alt text: say in it what the figure shows, for a reader who can't see it
            ARCHITECTURE.md:4:28: an image of `docs/chart.png`, which isn't an SVG in `docs/figures/`: draw it as one there, and embed that
            ARCHITECTURE.md:5:1: an image of `docs/figures/gone.svg`, which isn't in the repository: embed a figure that is, or remove the image
            docs/dev/guide.md:1:36: an image of `../../../good.svg`, which isn't an SVG in `docs/figures/`: draw it as one there, and embed that
            docs/dev/guide.md:1:67: an image of `https://example.com/a.svg`, which isn't an SVG in `docs/figures/`: draw it as one there, and embed that
            docs/dev/guide.md:2:81: an image of `../figures/good.svg` has no alt text: say in it what the figure shows, for a reader who can't see it
            docs/dev/guide.md:2:113: an image of `../chart.png`, which isn't an SVG in `docs/figures/`: draw it as one there, and embed that
            docs/figures/bad.svg: the figure has no `<title>`: add one that names what it shows, which a screen reader reads
            docs/figures/bad.svg: the figure has no `<desc>`: add one that says it in prose, which a screen reader reads
            docs/figures/bad.svg:2: the figure holds `@font-face`: remove it, as a figure is text and shapes alone, in the system's fonts
            docs/figures/bad.svg:3: the figure holds `<script`: remove it, as a figure is text and shapes alone, in the system's fonts
            docs/figures/bad.svg:4: the figure holds `<image`: remove it, as a figure is text and shapes alone, in the system's fonts
            docs/figures/bad.svg:4: the figure sets `Gone` as code, but nothing in the workspace is named so: name it as the code does now, or set it as prose
            docs/figures/chart.png: a file in `docs/figures/` that isn't an SVG: draw it as one, whose text stays text
            docs/figures/orphan.svg: no document embeds the figure: embed it beside the prose that states what it shows, or remove it"#]],
    );
}

#[test]
fn figure_of_crates() {
    let package = |name: &str, bin: bool, dependencies: &[(&str, Option<&str>)]| {
        let kind = if bin { "bin" } else { "lib" };
        let dependencies: Vec<_> = (dependencies.iter())
            .map(|(name, kind)| serde_json::json!({ "name": name, "kind": kind }))
            .collect();
        serde_json::json!({
            "name": name,
            "targets": [{ "name": name, "kind": [kind] }],
            "dependencies": dependencies,
        })
    };
    let metadata = serde_json::json!({ "packages": [
        package("app", true, &[("mid", None), ("side", None), ("helper", Some("dev")), ("serde", None)]),
        package("mid", false, &[("low", None), ("side", Some("build"))]),
        package("low", false, &[("base", None)]),
        package("base", false, &[]),
        package("side", false, &[]),
        package("tool", true, &[("side", None), ("base", None)]),
        package("helper", false, &[]),
    ]})
    .to_string();
    check(
        &[],
        |sh| {
            let stale = figure::check(sh, &metadata).unwrap_err();
            figure::write(sh, &metadata).unwrap();
            let fresh = figure::check(sh, &metadata);
            let drawn = sh.read_file(figure::PATH).unwrap();
            let drawn = drawn.replace(figure::STYLE, "<style>…</style>");
            format!("{stale}\n{fresh:?}\n{drawn}")
        },
        &expect![[r#"
            docs/figures/crates.svg is not what the workspace's manifests draw, as a crate or a dependency changed: run `cargo xtask figures`
            Ok(())
            <!-- cargo xtask figures writes this file from `cargo metadata`: edit xtask/src/figure.rs, not this -->
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 276 316" width="276" height="316" role="img" aria-labelledby="title desc">
            <title id="title">How the workspace's crates depend on one another</title>
            <desc id="desc">A box for each crate of the workspace, shaded if it builds a binary, in rows: an arrow leads from a crate down to each it depends on, but for a build or a development dependency, and for one that another of its arrows already leads to.</desc>
            <style>…</style>
            <defs><marker id="head" viewBox="0 0 8 8" refX="8" refY="4" markerWidth="8" markerHeight="8" orient="auto"><path class="head" d="M0 0L8 4L0 8z"/></marker></defs>
            <rect class="bg" width="276" height="316"/>
            <path class="arrow" d="M117 48C117 73 94 73 94 99" marker-end="url(#head)"/>
            <path class="arrow" d="M141 48C141 74 164 74 164 100V132C164 158 202 158 202 184V216C202 241 236 241 236 267" marker-end="url(#head)"/>
            <path class="arrow" d="M62 216C62 241 40 241 40 267" marker-end="url(#head)"/>
            <path class="arrow" d="M94 132C94 157 62 157 62 183" marker-end="url(#head)"/>
            <path class="arrow" d="M136 216C136 241 64 241 64 267" marker-end="url(#head)"/>
            <path class="arrow" d="M160 216C160 241 212 241 212 267" marker-end="url(#head)"/>
            <rect class="hot" x="93" y="16" width="72" height="32" rx="6"/>
            <text class="code mid" x="129" y="36">app</text>
            <rect class="box" x="58" y="100" width="72" height="32" rx="6"/>
            <text class="code mid" x="94" y="120">mid</text>
            <rect class="box" x="26" y="184" width="72" height="32" rx="6"/>
            <text class="code mid" x="62" y="204">low</text>
            <rect class="hot" x="112" y="184" width="72" height="32" rx="6"/>
            <text class="code mid" x="148" y="204">tool</text>
            <rect class="box" x="16" y="268" width="72" height="32" rx="6"/>
            <text class="code mid" x="52" y="288">base</text>
            <rect class="box" x="102" y="268" width="72" height="32" rx="6"/>
            <text class="code mid" x="138" y="288">helper</text>
            <rect class="box" x="188" y="268" width="72" height="32" rx="6"/>
            <text class="code mid" x="224" y="288">side</text>
            </svg>
        "#]],
    );
}

#[test]
fn test_data() {
    use Standing::{Deleted, Ignored, Tracked, Untracked};
    let file = |path| (path, "data\n".to_owned(), Tracked);
    check(
        &[
            file("crates/a/tests/it/main.rs"),
            file("crates/a/tests/it/records/outcome.json"),
            file("crates/a/tests/fixtures/README.md"),
            ("crates/a/tests/new.txt", "data\n".to_owned(), Untracked),
            (
                "crates/a/tests/ignored/kept.json",
                "data\n".to_owned(),
                Ignored,
            ),
            ("crates/a/tests/gone.json", "data\n".to_owned(), Deleted),
            file("crates/a/src/tests.json"),
            file("crates/a/tests.json"),
            file("crates/lang/tests/conformance/suite/LICENSE"),
            file("crates/lang/tests/conformance/suite/list.star"),
            file("crates/lang/tests/fixtures/lib/parts.star"),
            file("crates/lang/tests/fixtures/lib/parts.json"),
        ],
        tidy,
        &expect![[r#"
            crates/a/tests/new.txt: a file that isn't Rust, under a `tests/` directory: make what the test reads in the test, with Pumpkin's own writer where Pumpkin reads it
            crates/a/tests/fixtures/README.md: a file that isn't Rust, under a `tests/` directory: make what the test reads in the test, with Pumpkin's own writer where Pumpkin reads it
            crates/a/tests/it/records/outcome.json: a file that isn't Rust, under a `tests/` directory: make what the test reads in the test, with Pumpkin's own writer where Pumpkin reads it
            crates/lang/tests/fixtures/lib/parts.json: a file that isn't Rust, under a `tests/` directory: make what the test reads in the test, with Pumpkin's own writer where Pumpkin reads it"#]],
    );
}

#[test]
fn long_files() {
    use Standing::Tracked;
    let rust = |count| lines("fn f() {}", count);
    let ceilings = "[long_files]\n\
                    \"src/grown.rs\" = 1001\n\
                    \"src/held.rs\" = 1001\n\
                    \"src/shrunk.rs\" = 1002\n\
                    \"src/split.rs\" = 1001\n\
                    \"src/gone.rs\" = 1001\n";
    check(
        &[
            (".config/tidy.toml", ceilings.to_owned(), Tracked),
            ("src/at_cap.rs", rust(1000), Tracked),
            ("src/long.rs", rust(1001), Tracked),
            ("src/page/long.js", lines("f();", 1001), Tracked),
            ("crates/a/tests/it/long.rs", rust(1001), Tracked),
            ("docs/long.md", lines("A line.", 1001), Tracked),
            ("src/grown.rs", rust(1002), Tracked),
            ("src/held.rs", rust(1001), Tracked),
            ("src/shrunk.rs", rust(1001), Tracked),
            ("src/split.rs", rust(1000), Tracked),
        ],
        tidy,
        &expect![[r#"
            src/grown.rs: the file is 1002 lines, over the cap of 1000 and its ceiling of 1001 in .config/tidy.toml, which only goes down: put what is new in a module of its own, or split the file into modules that each hold one thing, in a pull request that only moves code
            src/long.rs: the file is 1001 lines, over the cap of 1000: put what is new in a module of its own, or split the file into modules that each hold one thing, in a pull request that only moves code
            src/page/long.js: the file is 1001 lines, over the cap of 1000: put what is new in a module of its own, or split the file into modules that each hold one thing, in a pull request that only moves code
            src/shrunk.rs: the file is 1001 lines, under its ceiling of 1002 in .config/tidy.toml: lower the ceiling to 1001
            src/split.rs: the file is 1000 lines, within the cap of 1000: take it off .config/tidy.toml
            .config/tidy.toml: `src/gone.rs` isn't a source file of the repository: take it off the list"#]],
    );
}

#[test]
fn diff() {
    use Standing::{Deleted, Ignored, Tracked, Untracked};
    let again = "cargo xtask mutants --in-diff HEAD";
    check(
        &[
            ("changed.rs", "fn f() {}\n".to_owned(), Tracked),
            ("deleted.rs", "fn d() {}\n".to_owned(), Deleted),
            ("new.rs", "fn g() {}\n".to_owned(), Untracked),
            ("notes.txt", "Notes.\n".to_owned(), Untracked),
            ("ignored/new.rs", "fn i() {}\n".to_owned(), Ignored),
        ],
        |sh| {
            sh.write_file("changed.rs", "fn f() {\n    g();\n}\n")
                .unwrap();
            let refused = diff::since(sh, "HEAD", &[], again).unwrap_err();
            cmd!(sh, "git add --intent-to-add -- new.rs")
                .quiet()
                .run()
                .unwrap();
            let diff = diff::since(sh, "HEAD", &[], again).unwrap();
            let below = diff::since(sh, "HEAD", &["changed.rs".into()], again).unwrap();
            format!("{refused}\n{diff}below changed.rs:\n{below}")
        },
        &expect![[r#"
            the diff would leave out untracked Rust files: add them to it with `git add --intent-to-add -- <repo>/new.rs`, then run `cargo xtask mutants --in-diff HEAD` again
            diff --git a/changed.rs b/changed.rs
            index 59760b5..e20511e 100644
            --- a/changed.rs
            +++ b/changed.rs
            @@ -1 +1,3 @@
            -fn f() {}
            +fn f() {
            +    g();
            +}
            diff --git a/deleted.rs b/deleted.rs
            deleted file mode 100644
            index d68b814..0000000
            --- a/deleted.rs
            +++ /dev/null
            @@ -1 +0,0 @@
            -fn d() {}
            diff --git a/new.rs b/new.rs
            new file mode 100644
            index 0000000..ed2b5d2
            --- /dev/null
            +++ b/new.rs
            @@ -0,0 +1 @@
            +fn g() {}
            below changed.rs:
            diff --git a/changed.rs b/changed.rs
            index 59760b5..e20511e 100644
            --- a/changed.rs
            +++ b/changed.rs
            @@ -1 +1,3 @@
            -fn f() {}
            +fn f() {
            +    g();
            +}
        "#]],
    );
}
