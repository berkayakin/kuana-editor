//! Trees extensions' code shows in the Find panel (0.2's `show-results`).
//! WIT has no nested records, so the code sends rows as a flat list, each
//! after its parent (`parent`: an earlier row's index); `tree` puts them
//! together on the extension's thread, capped, so the UI thread only turns a
//! bounded tree into the panel's nodes (`result_nodes`).

use std::{collections::HashMap, path::PathBuf};

use gpui::SharedString;

use super::{contrib::Contributions, ui_host::Tone};
use crate::{
    lsp::protocol::{Encoding, Position},
    results_panel::{self, Leading, ResultNode, ResultTarget},
};

/// The most rows one call shows; the rest become one "more" row.
pub const MAX_NODES: usize = 5_000;
/// How deep rows nest (the panel walks trees recursively).
pub const MAX_DEPTH: usize = 64;

/// A row with its children, as Kuana's own type (sent across threads).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultItem {
    pub label: String,
    pub detail: Option<String>,
    pub icon: Option<String>,
    pub tone: Tone,
    /// The file, 1-based line and 1-based column (in characters).
    pub place: Option<(PathBuf, u32, Option<u32>)>,
    pub children: Vec<ResultItem>,
}

/// Puts the flat rows (each with its parent's index) together as a tree:
/// rows past `MAX_NODES` are dropped for a "more" row at the end. An error
/// when a parent isn't an earlier row or rows nest past `MAX_DEPTH`.
pub fn tree(rows: Vec<(Option<u32>, ResultItem)>) -> Result<Vec<ResultItem>, String> {
    let more = rows.len().saturating_sub(MAX_NODES);
    let mut kids: Vec<Vec<usize>> = Vec::new();
    let mut depth: Vec<usize> = Vec::new();
    let mut roots = Vec::new();
    let mut items: Vec<Option<ResultItem>> = Vec::new();
    for (ix, (parent, item)) in rows.into_iter().take(MAX_NODES).enumerate() {
        match parent.map(|p| p as usize) {
            None => {
                roots.push(ix);
                depth.push(1);
            }
            Some(p) if p < ix => {
                let d = depth[p] + 1;
                if d > MAX_DEPTH {
                    return Err(format!("row {ix} nests deeper than {MAX_DEPTH} rows"));
                }
                kids[p].push(ix);
                depth.push(d);
            }
            Some(p) => return Err(format!("row {ix}: its parent {p} isn't an earlier row")),
        }
        kids.push(Vec::new());
        items.push(Some(item));
    }
    // Children come after their parents: from the end, each row's children
    // are complete when it's reached (no recursion, however deep).
    for ix in (0..items.len()).rev() {
        let children: Vec<ResultItem> = std::mem::take(&mut kids[ix]).into_iter().filter_map(|k| items[k].take()).collect();
        if let Some(item) = items[ix].as_mut() {
            item.children = children;
        }
    }
    let mut tree: Vec<ResultItem> = roots.into_iter().filter_map(|r| items[r].take()).collect();
    if more > 0 {
        tree.push(ResultItem { label: format!("{more} more not shown"), tone: Tone::Muted, ..Default::default() });
    }
    Ok(tree)
}

/// The panel's nodes for an extension's tree (groups open). Icons are looked
/// up once per name.
pub fn result_nodes(items: Vec<ResultItem>, extension: &Contributions) -> Vec<ResultNode> {
    fn node(item: ResultItem, extension: &Contributions, icons: &mut HashMap<String, Option<SharedString>>) -> ResultNode {
        let tone = match item.tone {
            Tone::Normal => results_panel::Tone::Normal,
            Tone::Muted => results_panel::Tone::Muted,
            Tone::Success => results_panel::Tone::Success,
            Tone::Warning => results_panel::Tone::Warning,
            Tone::Error => results_panel::Tone::Error,
        };
        let icon = item.icon.and_then(|name| icons.entry(name).or_insert_with_key(|name| crate::extension_view::icon_path(extension, name)).clone());
        let leading = match (icon, &item.place) {
            (Some(path), _) => Leading::Svg(path, tone),
            (None, Some((path, _, _))) if !item.children.is_empty() => Leading::File(path.clone()),
            (None, Some((_, line, _))) => Leading::Line(*line),
            (None, None) => Leading::Blank,
        };
        let target = item.place.map(|(path, line, column)| ResultTarget {
            path,
            position: Position { line: line.saturating_sub(1), character: column.unwrap_or(1).saturating_sub(1) },
            encoding: Encoding::Utf32,
        });
        let expanded = !item.children.is_empty();
        ResultNode {
            leading,
            label: item.label.into(),
            highlight: None,
            detail: item.detail.map(Into::into),
            target,
            open: None,
            children: item.children.into_iter().map(|c| node(c, extension, icons)).collect(),
            loader: None,
            expanded,
        }
    }
    let mut icons = HashMap::new();
    items.into_iter().map(|item| node(item, extension, &mut icons)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(parent: Option<u32>, label: &str) -> (Option<u32>, ResultItem) {
        (parent, ResultItem { label: label.into(), ..Default::default() })
    }

    fn labels(items: &[ResultItem]) -> String {
        items
            .iter()
            .map(|i| if i.children.is_empty() { i.label.clone() } else { format!("{}({})", i.label, labels(&i.children)) })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn builds_a_tree_from_rows_after_their_parents() {
        let rows = vec![row(None, "a"), row(Some(0), "a1"), row(None, "b"), row(Some(1), "a1x"), row(Some(0), "a2"), row(Some(2), "b1")];
        assert_eq!(labels(&tree(rows).unwrap()), "a(a1(a1x) a2) b(b1)");
        assert!(tree(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn refuses_parents_that_dont_come_first() {
        assert!(tree(vec![row(Some(0), "self")]).unwrap_err().contains("parent 0"));
        assert!(tree(vec![row(None, "a"), row(Some(5), "later")]).unwrap_err().contains("row 1"));
    }

    #[test]
    fn caps_rows_and_depth() {
        let mut rows = vec![row(None, "root")];
        rows.extend((1..MAX_NODES + 10).map(|_| row(Some(0), "x")));
        let built = tree(rows).unwrap();
        assert_eq!(built.len(), 2);
        assert_eq!(built[0].children.len(), MAX_NODES - 1);
        assert_eq!((built[1].label.as_str(), built[1].tone), ("10 more not shown", Tone::Muted));

        let chain = |n: usize| (0..n).map(|i| row(i.checked_sub(1).map(|p| p as u32), "x")).collect::<Vec<_>>();
        assert!(tree(chain(MAX_DEPTH)).is_ok());
        assert!(tree(chain(MAX_DEPTH + 1)).unwrap_err().contains("deeper"));
    }

    #[test]
    fn makes_panel_nodes_with_places() {
        let leaf = ResultItem { label: "use".into(), place: Some(("/p/a.rs".into(), 3, Some(5))), ..Default::default() };
        let file = ResultItem { label: "a.rs".into(), place: Some(("/p/a.rs".into(), 1, None)), children: vec![leaf], ..Default::default() };
        let nodes = result_nodes(vec![file], &Contributions::default());
        assert_eq!(nodes[0].leading, Leading::File("/p/a.rs".into()));
        assert!(nodes[0].expanded);
        let leaf = &nodes[0].children[0];
        assert_eq!(leaf.leading, Leading::Line(3));
        assert_eq!(leaf.target.as_ref().unwrap().position, Position { line: 2, character: 4 });
    }
}
