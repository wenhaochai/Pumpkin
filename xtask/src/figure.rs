//! The figure of how the workspace's crates depend on one another, `docs/figures/crates.svg`,
//! drawn from `cargo metadata`, so that it can't lag the manifests: `cargo xtask figures` writes
//! it, and `cargo xtask ci` fails while the committed one differs.
//!
//! Its layout is a layered drawing's, as Sugiyama's method makes one
//! (<https://en.wikipedia.org/wiki/Layered_graph_drawing>): each crate in a row, the crates it
//! depends on in rows below it, an edge that passes a row going through a gap of its own there,
//! and each row ordered to cross fewer edges.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use stdx::Context as _;

use crate::Result;

/// Where the figure is, from the repository's root.
pub(crate) const PATH: &str = "docs/figures/crates.svg";

const WRITE: &str = "cargo xtask figures";

/// The style that every figure in `docs/figures/` shares: its colours, those of GitHub's light
/// page, and, as the viewer's system prefers, its dark one's, whose background the figure fills
/// itself, so it reads on either page.
pub(crate) const STYLE: &str = r#"<style>
:root{--bg:#ffffff;--ink:#1f2328;--mute:#59636e;--line:#818b98;--card:#f6f8fa;--hot:#0969da;--hot-bg:#ddf4ff}
@media (prefers-color-scheme:dark){:root{--bg:#0d1117;--ink:#e6edf3;--mute:#9198a1;--line:#6e7681;--card:#151b23;--hot:#58a6ff;--hot-bg:#10243e}}
text{font:12.5px system-ui,-apple-system,"Segoe UI",Helvetica,Arial,sans-serif;fill:var(--ink)}
.code{font:600 13px ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
.name{font-size:13px;font-weight:600}
.note{font-size:11.5px;fill:var(--mute)}
.zone-label{font-size:10px;font-weight:700;letter-spacing:.8px;fill:var(--mute)}
.mid{text-anchor:middle}
.bg{fill:var(--bg)}
.box{fill:var(--card);stroke:var(--line);stroke-width:1.25}
.hot{fill:var(--hot-bg);stroke:var(--hot);stroke-width:1.5}
.zone{fill:none;stroke:var(--line);stroke-width:1.25;stroke-dasharray:6 4}
.arrow{fill:none;stroke:var(--line);stroke-width:1.5}
.head{fill:var(--line)}
</style>"#;

const MARKER: &str = r#"<defs><marker id="head" viewBox="0 0 8 8" refX="8" refY="4" markerWidth="8" markerHeight="8" orient="auto"><path class="head" d="M0 0L8 4L0 8z"/></marker></defs>"#;

// The figure's measures, in pixels: a crate's box is as wide as its name needs in 13 px
// monospace, about 8 px a character, with padding, and an edge passes a row in a gap of its own.
const MARGIN: usize = 16;
const BOX_HEIGHT: usize = 32;
const CHAR_WIDTH: usize = 8;
const BOX_PADDING: usize = 24;
const BOX_MIN_WIDTH: usize = 72;
const PASS_WIDTH: usize = 8;
const GAP: usize = 14;
const END_GAP: usize = 14;
const ROW_GAP: usize = 52;
const TOP: usize = 16;

/// How many times the rows are ordered, down then up, by where their neighbours are.
const SWEEPS: usize = 8;

/// Fails unless the figure at [`PATH`] below `root` is what `metadata`, the output of
/// `cargo metadata --format-version 1 --no-deps`, draws.
pub(crate) fn check(root: &Path, metadata: &str) -> Result<()> {
    let drawn = crates(metadata)?;
    let committed = fs::read_to_string(root.join(PATH)).unwrap_or_default();
    if committed == drawn {
        return Ok(());
    }
    Err(format!(
        "{PATH} is not what the workspace's manifests draw, as a crate or a dependency changed: \
         run `{WRITE}`"
    )
    .into())
}

/// Writes at [`PATH`] below `root` the figure that `metadata` draws, making its directory if need
/// be.
pub(crate) fn write(root: &Path, metadata: &str) -> Result<()> {
    let drawn = crates(metadata)?;
    let path = root.join(PATH);
    let written = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(&path, drawn));
    Ok(written.context(format_args!("writing {PATH} failed"))?)
}

/// A place in a row: a crate's box, or where the arrow `edges[n]` passes a row between its ends.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    Crate(usize),
    Pass(usize),
}

/// The figure's SVG: a box for each package of `metadata`, shaded if it builds a binary, and an
/// arrow for each of its dependencies on another package, but a build or a development one and one
/// that the other arrows imply.
fn crates(metadata: &str) -> Result<String> {
    let metadata: serde_json::Value = serde_json::from_str(metadata)
        .map_err(|error| format!("`cargo metadata` printed what isn't JSON: {error}"))?;
    let packages = metadata["packages"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let mut names: Vec<(&str, bool)> = Vec::new();
    for package in packages {
        let Some(name) = package["name"].as_str() else {
            return Err("`cargo metadata` gave a package without a name: expected one".into());
        };
        let targets = package["targets"].as_array().map_or(&[][..], Vec::as_slice);
        let binary = targets.iter().any(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.contains(&"bin".into()))
        });
        names.push((name, binary));
    }
    names.sort_unstable();
    let index: BTreeMap<&str, usize> = names
        .iter()
        .enumerate()
        .map(|(at, (name, _))| (*name, at))
        .collect();
    let mut direct: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); names.len()];
    for package in packages {
        let from = index[package["name"].as_str().unwrap_or_default()];
        let dependencies = package["dependencies"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        for dependency in dependencies {
            if let Some(&to) = dependency["name"].as_str().and_then(|name| index.get(name))
                && dependency["kind"].is_null()
            {
                direct[from].insert(to);
            }
        }
    }
    let edges = reduced(&direct);
    let layers = layers(names.len(), &edges);
    let rows = ordered(&layers, &edges);
    Ok(draw(&Drawing {
        names: &names,
        edges: &edges,
        layers: &layers,
        rows: &rows,
    }))
}

/// The edges of `direct` that no path of two edges or more implies: its transitive reduction.
fn reduced(direct: &[BTreeSet<usize>]) -> Vec<(usize, usize)> {
    let reaches = |from: usize, to: usize| {
        let mut seen = BTreeSet::new();
        let mut next = vec![from];
        while let Some(at) = next.pop() {
            if at == to {
                return true;
            }
            if seen.insert(at) {
                next.extend(&direct[at]);
            }
        }
        false
    };
    let mut edges = Vec::new();
    for (from, dependencies) in direct.iter().enumerate() {
        for &to in dependencies {
            let implied = dependencies
                .iter()
                .any(|&other| other != to && reaches(other, to));
            if !implied {
                edges.push((from, to));
            }
        }
    }
    edges
}

/// The layer of each of `count` crates, counted from the bottom: a crate above each one it
/// depends on, and as near the crates that depend on it as that allows, which keeps its arrows
/// short.
fn layers(count: usize, edges: &[(usize, usize)]) -> Vec<usize> {
    fn longest(at: usize, edges: &[(usize, usize)], memo: &mut [Option<usize>]) -> usize {
        if let Some(known) = memo[at] {
            return known;
        }
        let below = edges.iter().filter(|(from, _)| *from == at);
        let length = below
            .map(|&(_, to)| longest(to, edges, memo) + 1)
            .max()
            .unwrap_or(0);
        memo[at] = Some(length);
        length
    }
    let mut memo = vec![None; count];
    let longest: Vec<usize> = (0..count).map(|at| longest(at, edges, &mut memo)).collect();
    let mut by_length: Vec<usize> = (0..count).collect();
    by_length.sort_by_key(|&at| std::cmp::Reverse(longest[at]));
    let mut layers = longest.clone();
    for at in by_length {
        let above = edges.iter().filter(|(_, to)| *to == at);
        if let Some(lowest) = above.map(|&(from, _)| layers[from]).min() {
            layers[at] = lowest - 1;
        }
    }
    layers
}

/// Whether an arrow leads from `upper` to `lower`, an arrow's pass counting as a part of it.
fn joined(edges: &[(usize, usize)], upper: Slot, lower: Slot) -> bool {
    match (upper, lower) {
        (Slot::Crate(from), Slot::Crate(to)) => edges.contains(&(from, to)),
        (Slot::Crate(from), Slot::Pass(edge)) => edges[edge].0 == from,
        (Slot::Pass(edge), Slot::Crate(to)) => edges[edge].1 == to,
        (Slot::Pass(a), Slot::Pass(b)) => a == b,
    }
}

/// The slots of each row, the top one first, ordered so that few arrows cross: the order is
/// improved by sweeps down and up the rows, each slot set at the mean place of those it joins.
fn ordered(layers: &[usize], edges: &[(usize, usize)]) -> Vec<Vec<Slot>> {
    let top = layers.iter().copied().max().unwrap_or(0);
    let mut rows: Vec<Vec<Slot>> = vec![Vec::new(); top + 1];
    for (at, layer) in layers.iter().enumerate() {
        rows[top - layer].push(Slot::Crate(at));
    }
    for (edge, &(from, to)) in edges.iter().enumerate() {
        for row in &mut rows[(top - layers[from] + 1)..(top - layers[to])] {
            row.push(Slot::Pass(edge));
        }
    }
    for _ in 0..SWEEPS {
        for row in 1..rows.len() {
            let mean =
                |slot: &Slot| mean_place(&rows[row - 1], |other| joined(edges, *other, *slot));
            let keys: Vec<_> = rows[row].iter().map(mean).collect();
            sort_by_keys(&mut rows[row], &keys);
        }
        for row in (0..rows.len().saturating_sub(1)).rev() {
            let mean =
                |slot: &Slot| mean_place(&rows[row + 1], |other| joined(edges, *slot, *other));
            let keys: Vec<_> = rows[row].iter().map(mean).collect();
            sort_by_keys(&mut rows[row], &keys);
        }
    }
    rows
}

/// The mean index, in thousandths, of the slots of `row` that `joined` takes, if any.
fn mean_place(row: &[Slot], joined: impl Fn(&Slot) -> bool) -> Option<usize> {
    let places: Vec<usize> = (row.iter().enumerate())
        .filter(|(_, slot)| joined(slot))
        .map(|(at, _)| at * 1000)
        .collect();
    (!places.is_empty()).then(|| places.iter().sum::<usize>() / places.len())
}

/// Sorts `row` by `keys`, one for each slot, keeping a slot without one where it is, and slots
/// of equal keys in the order they had.
fn sort_by_keys(row: &mut Vec<Slot>, keys: &[Option<usize>]) {
    let mut keyed: Vec<(usize, Slot)> = Vec::new();
    for (at, (slot, key)) in row.iter().zip(keys).enumerate() {
        keyed.push((key.unwrap_or(at * 1000), *slot));
    }
    keyed.sort_by_key(|(key, _)| *key);
    *row = keyed.into_iter().map(|(_, slot)| slot).collect();
}

fn px(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The centre of each slot of `rows`, each as wide as `width` says, in its row's order and at
/// least [`GAP`] apart, each as near as that allows to the slots that `edges` join it to.
fn placed_centres(
    rows: &[Vec<Slot>],
    width: impl Fn(Slot) -> i64,
    edges: &[(usize, usize)],
) -> BTreeMap<(usize, Slot), i64> {
    let mut centres = BTreeMap::new();
    for (row, slots) in rows.iter().enumerate() {
        let mut x = 0;
        for &slot in slots {
            centres.insert((row, slot), x + width(slot) / 2);
            x += width(slot) + px(GAP);
        }
    }
    let order: Vec<usize> = (1..rows.len()).chain((0..rows.len()).rev()).collect();
    for _ in 0..SWEEPS {
        for &row in &order {
            let mut wanted = Vec::new();
            for &slot in &rows[row] {
                let mut beside = Vec::new();
                if row > 0 {
                    let above = rows[row - 1]
                        .iter()
                        .filter(|&&upper| joined(edges, upper, slot));
                    beside.extend(above.map(|&upper| centres[&(row - 1, upper)]));
                }
                if let Some(below) = rows.get(row + 1) {
                    let below = below.iter().filter(|&&lower| joined(edges, slot, lower));
                    beside.extend(below.map(|&lower| centres[&(row + 1, lower)]));
                }
                let mean = beside.iter().sum::<i64>() / px(beside.len().max(1));
                wanted.push(if beside.is_empty() {
                    centres[&(row, slot)]
                } else {
                    mean
                });
            }
            // Each slot as far right as the slot before it needs, then as far left as the slot
            // after it needs: their mean keeps the gaps of both.
            let slots = &rows[row];
            let apart = |at: usize| width(slots[at - 1]).midpoint(width(slots[at])) + px(GAP);
            let mut right = wanted.clone();
            for at in 1..slots.len() {
                right[at] = right[at].max(right[at - 1] + apart(at));
            }
            let mut left = wanted;
            for at in (1..slots.len()).rev() {
                left[at - 1] = left[at - 1].min(left[at] - apart(at));
            }
            for (at, &slot) in slots.iter().enumerate() {
                centres.insert((row, slot), right[at].midpoint(left[at]));
            }
        }
    }
    centres
}

/// A drawing's crates, `names`, each with whether it builds a binary, its `edges`, each crate's
/// layer and its rows.
struct Drawing<'a> {
    names: &'a [(&'a str, bool)],
    edges: &'a [(usize, usize)],
    layers: &'a [usize],
    rows: &'a [Vec<Slot>],
}

/// Where a slot's box or pass starts, and how wide it is, by its row and itself.
type Placed = BTreeMap<(usize, Slot), (usize, usize)>;

fn draw(drawing: &Drawing<'_>) -> String {
    let placed = drawing.placed();
    let (leave, meet) = drawing.ends(&placed);
    let rows = drawing.rows.len();
    let width = (placed.values().map(|(x, width)| x + width).max()).unwrap_or(0) + MARGIN;
    let height = TOP + rows * BOX_HEIGHT + rows.saturating_sub(1) * ROW_GAP + MARGIN;
    let mut svg = String::new();
    let _ = writeln!(
        svg,
        "<!-- {WRITE} writes this file from `cargo metadata`: edit xtask/src/figure.rs, not this -->"
    );
    let _ = writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} {height}" width="{width}" height="{height}" role="img" aria-labelledby="title desc">"#
    );
    svg.push_str("<title id=\"title\">How the workspace's crates depend on one another</title>\n");
    svg.push_str(
        "<desc id=\"desc\">A box for each crate of the workspace, shaded if it builds a binary, in \
         rows: an arrow leads from a crate down to each it depends on, but for a build or a \
         development dependency, and for one that another of its arrows already leads to.</desc>\n",
    );
    svg.push_str(STYLE);
    svg.push('\n');
    svg.push_str(MARKER);
    svg.push('\n');
    let _ = writeln!(
        svg,
        r#"<rect class="bg" width="{width}" height="{height}"/>"#
    );
    for (edge, &(from, to)) in drawing.edges.iter().enumerate() {
        let (mut x, mut y) = (leave[edge], top_of(drawing.row_of(from)) + BOX_HEIGHT);
        let mut path = format!("M{x} {y}");
        for row in drawing.row_of(from) + 1..drawing.row_of(to) {
            let (pass, _) = placed[&(row, Slot::Pass(edge))];
            let pass = pass + PASS_WIDTH / 2;
            let row_top = top_of(row);
            let _ = write!(
                path,
                "{}V{}",
                curve((x, y), (pass, row_top)),
                row_top + BOX_HEIGHT
            );
            (x, y) = (pass, row_top + BOX_HEIGHT);
        }
        // The arrowhead's tip ends a pixel short of the box's edge, which its stroke covers.
        let tip = (meet[edge], top_of(drawing.row_of(to)) - 1);
        let _ = write!(path, "{}", curve((x, y), tip));
        let _ = writeln!(
            svg,
            r#"<path class="arrow" d="{path}" marker-end="url(#head)"/>"#
        );
    }
    for ((row, slot), (x, width)) in &placed {
        let Slot::Crate(at) = slot else { continue };
        let (name, binary) = drawing.names[*at];
        let y = top_of(*row);
        let class = if binary { "hot" } else { "box" };
        let _ = writeln!(
            svg,
            r#"<rect class="{class}" x="{x}" y="{y}" width="{width}" height="{BOX_HEIGHT}" rx="6"/>"#
        );
        let (middle, baseline) = (x + width / 2, y + BOX_HEIGHT / 2 + 4);
        let _ = writeln!(
            svg,
            r#"<text class="code mid" x="{middle}" y="{baseline}">{name}</text>"#
        );
    }
    svg.push_str("</svg>\n");
    svg
}

fn top_of(row: usize) -> usize {
    TOP + row * (BOX_HEIGHT + ROW_GAP)
}

impl Drawing<'_> {
    /// The row of crate `at`, counted from the top.
    fn row_of(&self, at: usize) -> usize {
        self.rows.len() - 1 - self.layers[at]
    }

    fn width_of(&self, slot: Slot) -> usize {
        match slot {
            // Wide enough for the name, and for a point of its own for each arrow that leaves
            // the box or meets it.
            Slot::Crate(at) => {
                let leaving = self.edges.iter().filter(|(from, _)| *from == at).count();
                let meeting = self.edges.iter().filter(|(_, to)| *to == at).count();
                let name = self.names[at].0.len() * CHAR_WIDTH + BOX_PADDING;
                name.max(BOX_MIN_WIDTH)
                    .max((leaving.max(meeting) + 1) * END_GAP)
            }
            Slot::Pass(_) => PASS_WIDTH,
        }
    }

    /// Where each slot is, the leftmost [`MARGIN`] from the left.
    fn placed(&self) -> Placed {
        let width = |slot| px(self.width_of(slot));
        let centres = placed_centres(self.rows, width, self.edges);
        let lefts = centres
            .iter()
            .map(|(&(_, slot), centre)| centre - width(slot) / 2);
        let left = lefts.min().unwrap_or(0);
        let mut placed = Placed::new();
        for (&(row, slot), centre) in &centres {
            let x = usize::try_from(centre - width(slot) / 2 - left).unwrap_or(0) + MARGIN;
            placed.insert((row, slot), (x, self.width_of(slot)));
        }
        placed
    }

    /// The x at which each arrow leaves its upper crate and the x at which it meets its lower one,
    /// spread along the box in the order of where the arrows go next, so that they don't cross
    /// there.
    fn ends(&self, placed: &Placed) -> (Vec<usize>, Vec<usize>) {
        let next_x = |edge: usize, leaving: bool| {
            let (from, to) = self.edges[edge];
            let (row, end) = if leaving {
                (self.row_of(from) + 1, to)
            } else {
                (self.row_of(to) - 1, from)
            };
            let slot = if row == self.row_of(end) {
                Slot::Crate(end)
            } else {
                Slot::Pass(edge)
            };
            let (x, width) = placed[&(row, slot)];
            x + width / 2
        };
        let mut leave = vec![0; self.edges.len()];
        let mut meet = vec![0; self.edges.len()];
        for at in 0..self.names.len() {
            for (leaving, ends) in [(true, &mut leave), (false, &mut meet)] {
                let own = |edge: &usize| {
                    let (from, to) = self.edges[*edge];
                    if leaving { from == at } else { to == at }
                };
                let mut own: Vec<usize> = (0..self.edges.len()).filter(own).collect();
                own.sort_by_key(|&edge| next_x(edge, leaving));
                let (x, width) = placed[&(self.row_of(at), Slot::Crate(at))];
                for (place, &edge) in own.iter().enumerate() {
                    ends[edge] = x + width * (place + 1) / (own.len() + 1);
                }
            }
        }
        (leave, meet)
    }
}

/// A curve from `from` down to `to` that leaves and arrives upright.
fn curve((x1, y1): (usize, usize), (x2, y2): (usize, usize)) -> String {
    let middle = y1.midpoint(y2);
    format!("C{x1} {middle} {x2} {middle} {x2} {y2}")
}
