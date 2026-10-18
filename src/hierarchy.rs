//! Call Hierarchy and Type Hierarchy in the Find panel, from the language
//! server's `callHierarchy/*` and `typeHierarchy/*` requests. The symbol at
//! the caret is the root, with two groups under it: its callers and what it
//! calls, or its supertypes and subtypes. Each item in them opens onto the
//! same again for itself, asked for only when it opens (`Loader`), since a
//! hierarchy can go on for ever (recursion, `Object`).

use std::{path::Path, rc::Rc, sync::Arc};

use serde_json::{Value, json};

use crate::{
    assets::Icon,
    lsp::{
        Server,
        protocol::{Encoding, LspRange, uri_to_path},
    },
    results_panel::{Leading, Loader, ResultNode, ResultTarget},
};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Call,
    Type,
}

impl Kind {
    pub fn capability(self) -> &'static str {
        match self {
            Kind::Call => "callHierarchyProvider",
            Kind::Type => "typeHierarchyProvider",
        }
    }

    pub fn prepare(self) -> &'static str {
        match self {
            Kind::Call => "textDocument/prepareCallHierarchy",
            Kind::Type => "textDocument/prepareTypeHierarchy",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Call => "Call Hierarchy",
            Kind::Type => "Type Hierarchy",
        }
    }

    fn directions(self) -> [(Direction, bool); 2] {
        // What opens at first: who calls it (JetBrains' default); both sides of a type.
        match self {
            Kind::Call => [(Direction::Callers, true), (Direction::Callees, false)],
            Kind::Type => [(Direction::Supertypes, true), (Direction::Subtypes, true)],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Direction {
    Callers,
    Callees,
    Supertypes,
    Subtypes,
}

impl Direction {
    fn method(self) -> &'static str {
        match self {
            Direction::Callers => "callHierarchy/incomingCalls",
            Direction::Callees => "callHierarchy/outgoingCalls",
            Direction::Supertypes => "typeHierarchy/supertypes",
            Direction::Subtypes => "typeHierarchy/subtypes",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Direction::Callers => "Callers",
            Direction::Callees => "Calls",
            Direction::Supertypes => "Supertypes",
            Direction::Subtypes => "Subtypes",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Direction::Callers | Direction::Supertypes => Icon::ArrowUp,
            Direction::Callees | Direction::Subtypes => Icon::ArrowDown,
        }
    }
}

/// Names a file as the panel shows it (relative to the project).
pub type Labeler = Rc<dyn Fn(&Path) -> String>;

/// Makes the loader for an item's children in a direction (none in tests).
type MakeLoader<'a> = &'a dyn Fn(Direction, &Value) -> Option<Loader>;

/// The trees for what `prepare` returned: each item with its two groups,
/// the first ones open (the panel loads them right away).
pub fn roots(kind: Kind, server: Arc<Server>, items: Vec<Value>, label: Labeler) -> Vec<ResultNode> {
    let encoding = server.encoding;
    let make = |direction, item: &Value| Some(loader(direction, server.clone(), item.clone(), label.clone()));
    items
        .iter()
        .filter_map(|item| {
            let mut root = item_node(item, None, None, encoding, &label)?;
            root.children = kind
                .directions()
                .into_iter()
                .map(|(direction, open)| ResultNode {
                    leading: Leading::Icon(direction.icon()),
                    label: direction.label().into(),
                    highlight: None,
                    detail: None,
                    target: None,
                    open: None,
                    children: Vec::new(),
                    loader: make(direction, item),
                    expanded: open,
                })
                .collect();
            root.expanded = true;
            Some(root)
        })
        .collect()
}

/// Asks the server for an item's callers (or callees, supertypes, subtypes),
/// each of which opens onto the same in turn.
fn loader(direction: Direction, server: Arc<Server>, item: Value, label: Labeler) -> Loader {
    Loader::new(move |cx| {
        let (server, item, label) = (server.clone(), item.clone(), label.clone());
        cx.spawn(async move |_| {
            let value = server.request(direction.method(), json!({ "item": item })).await.map_err(|e| e.message)?;
            let make = |direction, item: &Value| Some(loader(direction, server.clone(), item.clone(), label.clone()));
            Ok(children(direction, &value, server.encoding, &label, &make))
        })
    })
}

/// A direction's answer as nodes. A caller opens at its first call (where
/// it calls the item); a callee, supertype or subtype at its name.
fn children(direction: Direction, value: &Value, encoding: Encoding, label: &Labeler, make: MakeLoader) -> Vec<ResultNode> {
    let Some(entries) = value.as_array() else { return Vec::new() };
    entries
        .iter()
        .filter_map(|entry| {
            let (item, at, calls) = match direction {
                Direction::Callers => {
                    let ranges = entry["fromRanges"].as_array().map_or(&[][..], Vec::as_slice);
                    (&entry["from"], ranges.first().and_then(LspRange::from_json), ranges.len())
                }
                Direction::Callees => (&entry["to"], None, 0),
                Direction::Supertypes | Direction::Subtypes => (entry, None, 0),
            };
            let note = (calls > 1).then(|| format!("{calls} calls"));
            let mut node = item_node(item, at, note, encoding, label)?;
            node.loader = make(direction, item);
            Some(node)
        })
        .collect()
}

/// A hierarchy item (`CallHierarchyItem` / `TypeHierarchyItem`): its name,
/// then where it is; it opens at `at` or its name.
fn item_node(item: &Value, at: Option<LspRange>, note: Option<String>, encoding: Encoding, label: &Labeler) -> Option<ResultNode> {
    let name = item["name"].as_str()?;
    let path = uri_to_path(item["uri"].as_str()?)?;
    let range = at.or_else(|| LspRange::from_json(&item["selectionRange"])).or_else(|| LspRange::from_json(&item["range"]))?;
    let mut detail = format!("{}:{}", label(&path), range.start.line + 1);
    if let Some(note) = note {
        detail = format!("{detail}  ·  {note}");
    }
    Some(ResultNode {
        leading: Leading::File(path.clone()),
        label: name.to_string().into(),
        highlight: None,
        detail: Some(detail.into()),
        target: Some(ResultTarget { path, position: range.start, encoding }),
        open: None,
        children: Vec::new(),
        loader: None,
        expanded: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::protocol::Position;

    fn item(name: &str, line: u32) -> Value {
        let range = json!({ "start": { "line": line, "character": 7 }, "end": { "line": line, "character": 12 } });
        json!({ "name": name, "kind": 12, "uri": "file:///p/src/util.rs", "range": range, "selectionRange": range })
    }

    #[test]
    fn callers_open_at_their_call_and_count_calls() {
        let label: Labeler = Rc::new(|p: &Path| p.strip_prefix("/p").unwrap().display().to_string());
        let call = |line| json!({ "start": { "line": line, "character": 4 }, "end": { "line": line, "character": 9 } });
        let value = json!([
            { "from": item("twice", 4), "fromRanges": [call(5), call(5)] },
            { "from": item("main", 9), "fromRanges": [call(10)] },
            { "from": { "name": "broken" }, "fromRanges": [] },
        ]);
        let opened = std::cell::RefCell::new(Vec::new());
        let make = |direction, item: &Value| {
            opened.borrow_mut().push((direction, item["name"].as_str().unwrap_or_default().to_string()));
            None
        };
        let nodes = children(Direction::Callers, &value, Encoding::Utf16, &label, &make);
        assert_eq!(nodes.len(), 2);
        assert_eq!((nodes[0].label.as_ref(), nodes[0].detail.as_ref().map(|d| d.as_ref())), ("twice", Some("src/util.rs:6  ·  2 calls")));
        assert_eq!(nodes[0].target.as_ref().map(|t| t.position), Some(Position { line: 5, character: 4 }));
        assert_eq!(nodes[1].detail.as_ref().map(|d| d.as_ref()), Some("src/util.rs:11"));
        // Each caller opens onto its own callers.
        assert_eq!(opened.into_inner(), [(Direction::Callers, "twice".into()), (Direction::Callers, "main".into())]);

        // Callees and types open at their names.
        let callees = children(Direction::Callees, &json!([{ "to": item("greet", 0), "fromRanges": [call(5)] }]), Encoding::Utf8, &label, &|_, _| None);
        assert_eq!(callees[0].target.as_ref().map(|t| t.position), Some(Position { line: 0, character: 7 }));
        let supertypes = children(Direction::Supertypes, &json!([item("Shape", 2)]), Encoding::Utf8, &label, &|_, _| None);
        assert_eq!(supertypes[0].label.as_ref(), "Shape");
        assert!(children(Direction::Subtypes, &Value::Null, Encoding::Utf8, &label, &|_, _| None).is_empty());
    }
}
