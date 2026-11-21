//! The formatter of `.star` files, `buildifier` beside `pumpkin`: `pumpkin fmt` formats files with
//! it, and `pumpkin check` lints a task's files with it.
//!
//! The formatter is buildifier, Bazel's formatter of Starlark, and this module alone knows it:
//! each file's text goes to it on its standard input, and what it gives back is read here, so
//! that none of its flags or forms is `pumpkin`'s interface, and another formatter could take its
//! place.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Output, Stdio};
use std::{fs, io};

use lang::Finding;
use serde::Deserialize;
use stdx::process::Environment;

use crate::{absolute_utf8, cli, next, say};

// No configuration file, so that every `.star` file is written alike, wherever it is; and each
// text is read as a generic Starlark file, not one of Bazel's, whatever its name.
const ALWAYS: [&str; 2] = ["--config=off", "--type=default"];

/// The lints that a file can raise, by the formatter's name for each, with the message that
/// says what is wrong and what to do. The formatter's other lints are of Bazel's own files, or
/// of what the language refuses before any lint.
const LINTS: [(&str, &str); 19] = [
    (
        "load",
        "this name is loaded, but never used: drop it from the `load`",
    ),
    (
        "unused-variable",
        "this is bound, but never used: use it, or drop it",
    ),
    (
        "no-effect",
        "this expression's value is never used: use it, or drop the statement",
    ),
    (
        "uninitialized",
        "this name may be unbound here, as a path to this use doesn't bind it: bind it on every \
         path to this use",
    ),
    (
        "unreachable",
        "this statement is never reached, as one before it ends its block: drop it, or move it \
         before that one",
    ),
    (
        "return-value",
        "this function returns a value on some paths, but not on others: return one on every \
         path, or on none",
    ),
    (
        "module-docstring",
        "the file has no docstring: begin it with a string that says what it holds",
    ),
    (
        "function-docstring",
        "this function has no docstring: begin its body with a string that says what it does",
    ),
    (
        "function-docstring-header",
        "this docstring doesn't begin with a summary of one line: write one, then a blank line",
    ),
    (
        "function-docstring-args",
        "this docstring's `Args:` doesn't name the function's parameters: give each a line, and \
         no other name",
    ),
    (
        "function-docstring-return",
        "this docstring doesn't say what the function returns: say it under `Returns:`",
    ),
    (
        "name-conventions",
        "this name breaks the naming convention: name a variable in lower_snake_case, and a \
         constant in UPPER_SNAKE_CASE",
    ),
    (
        "confusing-name",
        "this name reads as another character: name it other than `l`, `I` or `O`",
    ),
    (
        "skylark-comment",
        "this comment names the language by its old name: say \"Starlark\"",
    ),
    (
        "skylark-docstring",
        "this docstring names the language by its old name: say \"Starlark\"",
    ),
    (
        "string-iteration",
        "this iterates over a string, which Starlark refuses: iterate over its indices, or over \
         a list",
    ),
    (
        "dict-concatenation",
        "this adds dicts with `+`, which Starlark refuses: make one dict, as a comprehension does",
    ),
    (
        "dict-method-named-arg",
        "this passes `default` to a dict's method by name, which it takes by position alone: pass \
         it by position",
    ),
    (
        "keyword-positional-params",
        "this passes by name a parameter that is positional: pass it by position",
    ),
];

#[derive(Deserialize)]
struct Report {
    files: Vec<Linted>,
}

#[derive(Deserialize)]
struct Linted {
    /// Whether the formatter reads the file as Starlark.
    valid: bool,
    warnings: Vec<Warning>,
}

#[derive(Deserialize)]
struct Warning {
    category: String,
    start: Position,
    end: Position,
}

#[derive(Deserialize)]
struct Position {
    line: usize,
    column: usize,
}

impl Position {
    /// The byte offset in `text` of this position, whose line and column count from 1, a column in
    /// characters; a column one past a line's last character is the line's end. None if `text` has
    /// no such place.
    fn offset(&self, text: &str) -> Option<usize> {
        let start = match self.line.checked_sub(1)? {
            0 => 0,
            before => text.match_indices('\n').nth(before - 1)?.0 + 1,
        };
        let line = &text[start..];
        let line = line.find('\n').map_or(line, |end| &line[..end]);
        let mut columns = line.char_indices().map(|(at, _)| at).chain([line.len()]);
        Some(start + columns.nth(self.column.checked_sub(1)?)?)
    }
}

pub(crate) struct Formatter(PathBuf);

impl Formatter {
    /// The formatter installed beside this `pumpkin`.
    pub(crate) fn beside() -> Result<Self, String> {
        let pumpkin = crate::workspace::exe("expected the formatter beside it")?;
        let formatter = pumpkin.with_file_name(install::File::Buildifier.name());
        if !formatter.is_file() {
            return Err(format!(
                "{} doesn't exist: expected the formatter of `.star` files beside `pumpkin`, as \
                 `cargo xtask build` puts it",
                formatter.display()
            ));
        }
        Ok(Self(formatter))
    }

    /// `text` formatted; or, if the formatter reads it as no Starlark, the line and column where
    /// it says so, if it says. The outer error is a failure of the formatter.
    fn format(&self, text: &str) -> Result<Result<String, Option<(usize, usize)>>, String> {
        let output = self.run(&["--mode=fix"], text)?;
        match output.status.code() {
            Some(0) => String::from_utf8(output.stdout)
                .map(Ok)
                .map_err(|_| "the formatter wrote a text that isn't UTF-8".to_owned()),
            Some(1) => {
                // Its first line is `<stdin>:<line>:<column>: <message>`.
                let stderr = String::from_utf8_lossy(&output.stderr);
                let place = stderr.strip_prefix("<stdin>:").and_then(|rest| {
                    let mut numbers = rest.splitn(3, ':').map(str::parse::<usize>);
                    Some((numbers.next()?.ok()?, numbers.next()?.ok()?))
                });
                Ok(Err(place))
            }
            _ => Err(self.failed(&output)),
        }
    }

    /// The lints that `text` raises, in the order of the text. It fails if the formatter doesn't
    /// report on `text` as it is to: it doesn't read it as Starlark, or reports a lint that
    /// [`LINTS`] lacks or a place that `text` lacks.
    pub(crate) fn lint(&self, text: &str) -> Result<Vec<Finding>, String> {
        let args = ["--mode=check", "--lint=warn", "--format=json"];
        let output = self.run(&args, text)?;
        if !output.status.success() {
            return Err(self.failed(&output));
        }
        let report: Report = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("the formatter's report isn't what is expected: {error}"))?;
        let Report { files } = report;
        let [Linted { valid, warnings }] = <[Linted; 1]>::try_from(files)
            .map_err(|files| format!("the formatter reported {} files, not 1", files.len()))?;
        if !valid {
            return Err(
                "the formatter doesn't read the file as Starlark, as `pumpkin` does".into(),
            );
        }
        let mut findings = Vec::new();
        for Warning {
            category,
            start,
            end,
        } in warnings
        {
            let Some((_, message)) = LINTS.iter().find(|(name, _)| *name == category) else {
                return Err(format!(
                    "the formatter reported the lint `{category}`, which no file that `pumpkin` \
                     evaluates can have: expected one of the lints that `pumpkin` says"
                ));
            };
            let (Some(from), Some(to)) = (start.offset(text), end.offset(text)) else {
                return Err(format!(
                    "the formatter reported `{category}` at {}:{}, which the file doesn't have",
                    start.line, start.column
                ));
            };
            let span = from..to.max(from);
            findings.push(Finding {
                span,
                message: (*message).to_owned(),
            });
        }
        findings.sort_by_key(|finding| (finding.span.start, finding.span.end));
        Ok(findings)
    }

    fn run(&self, args: &[&str], text: &str) -> Result<Output, String> {
        // No variable of this process's environment, which could steer the formatter.
        let mut command = stdx::process::command(&self.0, Environment::Only(&[]));
        command.args(ALWAYS).args(args);
        let spawned = (command.stdin(Stdio::piped()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child =
            spawned.map_err(|error| format!("{} can't be run: {error}", self.0.display()))?;
        // The formatter reads all of its input before it writes, so writing all of it first never
        // waits on a pipe that is full.
        let written = child
            .stdin
            .take()
            .map(|mut stdin| stdin.write_all(text.as_bytes()));
        let output = child.wait_with_output();
        let output =
            output.map_err(|error| format!("waiting for the formatter failed: {error}"))?;
        // A formatter that failed may have stopped reading first: its failure is the one to say.
        match written {
            Some(Err(error)) if output.status.success() => {
                Err(format!("writing to the formatter failed: {error}"))
            }
            Some(Ok(()) | Err(_)) | None => Ok(output),
        }
    }

    fn failed(&self, output: &Output) -> String {
        let stderr = String::from_utf8_lossy(&output.stderr);
        format!(
            "the formatter {} failed with {}: {}",
            self.0.display(),
            output.status,
            stderr.trim_end()
        )
    }
}

pub(crate) fn fmt(flags: cli::Fmt) -> Result<ExitCode, String> {
    let cli::Fmt { paths, check } = flags;
    let formatter = Formatter::beside()?;
    let mut files = Vec::new();
    if paths.is_empty() {
        star_files(Path::new("."), &mut files)?;
        // Said by their paths below the working directory, as `.` adds nothing to them.
        for file in &mut files {
            if let Ok(below) = file.strip_prefix(".") {
                *file = below.to_owned();
            }
        }
    }
    for path in &paths {
        star_files(Path::new(path), &mut files)?;
    }
    let mut failed = Vec::new();
    for file in files {
        let problem = match formatted(&formatter, &file, check) {
            Ok(false) if check => format!("{}: the file isn't formatted", file.display()),
            Ok(_) => continue,
            Err(problem) => problem,
        };
        say(problem);
        failed.push(file);
    }
    if failed.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let failed: Result<Vec<String>, String> = (failed.iter())
        .map(|file| match file.to_str() {
            Some(file) => absolute_utf8(file, "the file"),
            None => Err(format!(
                "{} isn't UTF-8, as the commands that `pumpkin` says to run next name it: give \
                 it a name that is",
                file.display()
            )),
        })
        .collect();
    let failed = failed?;
    let them = if failed.len() == 1 { "it" } else { "them" };
    let again = if check { "" } else { " again" };
    Err(format!(
        "to format {them}{again}: {}",
        next::again("fmt", &[], &failed)
    ))
}

/// Whether `file` was already formatted; if not, rewrites it, unless `check`.
fn formatted(formatter: &Formatter, file: &Path, check: bool) -> Result<bool, String> {
    let shown = file.display();
    let text =
        fs::read(file).map_err(|error| format!("{shown}: the file can't be read: {error}"))?;
    let text = String::from_utf8(text)
        .map_err(|_| format!("{shown}: the file isn't UTF-8, as Starlark is: write it in UTF-8"))?;
    let rewritten = match formatter.format(&text)? {
        Ok(rewritten) => rewritten,
        Err(place) => {
            let (place, here) = match place {
                Some((line, column)) => (format!(":{line}:{column}"), " here"),
                None => (String::new(), ""),
            };
            return Err(format!(
                "{shown}{place}: the file isn't valid Starlark{here}, so it can't be formatted: \
                 make it valid Starlark"
            ));
        }
    };
    if rewritten == text {
        return Ok(true);
    }
    if !check {
        fs::write(file, rewritten)
            .map_err(|error| format!("{shown}: the file can't be written: {error}"))?;
    }
    Ok(false)
}

fn star_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let shown = path.display();
    let metadata = fs::metadata(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => {
            format!("{shown} doesn't exist: name a `.star` file, or a directory, that does")
        }
        _ => format!("{shown} can't be looked up: {error}"),
    })?;
    if metadata.is_dir() {
        return below(path, files);
    }
    if !(metadata.is_file() && is_star(path)) {
        return Err(format!(
            "{shown} is neither a `.star` file nor a directory: name one"
        ));
    }
    files.push(path.to_owned());
    Ok(())
}

fn below(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let read = |error| format!("{} can't be read: {error}", dir.display());
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(read)? {
        let entry = entry.map_err(read)?;
        // The kind of the entry itself: a symbolic link is neither of these.
        let kind = entry.file_type().map_err(read)?;
        entries.push((entry.file_name(), kind));
    }
    // So that the files, and their problems, come in an order of their own, not the directory's.
    entries.sort_by(|(one, _), (other, _)| one.cmp(other));
    for (name, kind) in entries {
        if name.as_encoded_bytes().starts_with(b".") {
            continue;
        }
        let path = dir.join(name);
        if kind.is_dir() {
            below(&path, files)?;
        } else if kind.is_file() && is_star(&path) {
            files.push(path);
        }
    }
    Ok(())
}

fn is_star(path: &Path) -> bool {
    path.extension() == Some("star".as_ref())
}
