//! JavaScript and TypeScript linting on top of the oxc parser and semantic model.

use std::collections::HashSet;

use oxc_allocator::Allocator;
use oxc_ast::{
    AstKind,
    ast::{BinaryOperator, Expression, ObjectPropertyKind, UnaryOperator, VariableDeclarationKind},
};
use oxc_diagnostics::OxcDiagnostic;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType, Span};
use oxc_syntax::symbol::SymbolFlags;

use super::{Diagnostic, Severity};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Context {
    /// A standalone `.js` file.
    File,
    /// An inline `<script>` in an HTML page. Top-level declarations are
    /// globals that markup (e.g. `onclick`) may use, so they are not reported
    /// as unused.
    InlineScript,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    JavaScript,
    TypeScript,
    Tsx,
    /// A `.d.ts` file: only declarations, so only syntax is checked.
    Declaration,
}

impl Dialect {
    fn source_type(self) -> SourceType {
        match self {
            Dialect::JavaScript => SourceType::unambiguous().with_jsx(true),
            Dialect::TypeScript => SourceType::ts(),
            Dialect::Tsx => SourceType::tsx(),
            Dialect::Declaration => SourceType::d_ts(),
        }
    }
}

pub fn lint(source: &str, base: usize, context: Context, dialect: Dialect) -> Vec<Diagnostic> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, dialect.source_type()).parse();
    let typescript = dialect != Dialect::JavaScript;

    let mut out = Vec::new();
    let mut push = |span: Span, severity, message: String, rule: &'static str| {
        out.push(Diagnostic {
            range: base + span.start as usize..base + span.end as usize,
            severity,
            message: message.into(),
            rule: rule.into(),
            unnecessary: false,
        });
    };

    for d in parsed.diagnostics.iter() {
        push(diagnostic_span(d), Severity::Error, clean_message(d), "syntax");
    }
    if parsed.panicked {
        return out;
    }

    let built = SemanticBuilder::new()
        .with_build_nodes(true)
        .with_check_syntax_error(true)
        .build(&parsed.program);
    for d in built.diagnostics.iter() {
        push(diagnostic_span(d), Severity::Error, clean_message(d), "syntax");
    }
    if dialect == Dialect::Declaration {
        return out;
    }

    let semantic = built.semantic;
    let nodes = semantic.nodes();
    let scoping = semantic.scoping();

    for node in nodes.iter() {
        match node.kind() {
            AstKind::DebuggerStatement(stmt) => push(
                stmt.span,
                Severity::Warning,
                "Unexpected 'debugger' statement".into(),
                "no-debugger",
            ),
            AstKind::VariableDeclaration(decl) if decl.kind == VariableDeclarationKind::Var => {
                push(
                    Span::new(decl.span.start, decl.span.start + 3),
                    Severity::Info,
                    "Unexpected var, use let or const instead".into(),
                    "no-var",
                )
            }
            AstKind::BinaryExpression(bin) => {
                let loose = matches!(
                    bin.operator,
                    BinaryOperator::Equality | BinaryOperator::Inequality
                );
                let strict = matches!(
                    bin.operator,
                    BinaryOperator::StrictEquality | BinaryOperator::StrictInequality
                );
                let is_nan = |e: &Expression| {
                    matches!(e, Expression::Identifier(id) if id.name == "NaN")
                };
                if (loose || strict) && (is_nan(&bin.left) || is_nan(&bin.right)) {
                    push(
                        bin.span,
                        Severity::Error,
                        "Use Number.isNaN() to compare with NaN".into(),
                        "use-isnan",
                    );
                } else if loose {
                    let is_null = |e: &Expression| matches!(e, Expression::NullLiteral(_));
                    let is_typeof = |e: &Expression| {
                        matches!(e, Expression::UnaryExpression(u) if u.operator == UnaryOperator::Typeof)
                    };
                    // `x == null` is an accepted idiom; typeof comparisons are always strings.
                    let exempt = is_null(&bin.left)
                        || is_null(&bin.right)
                        || is_typeof(&bin.left)
                        || is_typeof(&bin.right);
                    if !exempt {
                        let op = if bin.operator == BinaryOperator::Equality {
                            ("==", "===")
                        } else {
                            ("!=", "!==")
                        };
                        let op_span = operator_span(source, bin.left.span().end, bin.right.span().start, op.0)
                            .unwrap_or(bin.span);
                        push(
                            op_span,
                            Severity::Warning,
                            format!("Expected '{}' and instead saw '{}'", op.1, op.0),
                            "eqeqeq",
                        );
                    }
                }
            }
            AstKind::ObjectExpression(obj) => {
                let mut seen = HashSet::new();
                for prop in &obj.properties {
                    let ObjectPropertyKind::ObjectProperty(prop) = prop else {
                        // A spread resets what can be considered a duplicate.
                        seen.clear();
                        continue;
                    };
                    if prop.computed {
                        continue;
                    }
                    if let Some(name) = prop.key.static_name() {
                        let key = (name.to_string(), prop.kind as u8);
                        if !seen.insert(key) {
                            push(
                                prop.key.span(),
                                Severity::Error,
                                format!("Duplicate key '{name}'"),
                                "no-dupe-keys",
                            );
                        }
                    }
                }
            }
            AstKind::IfStatement(stmt) => {
                if let Expression::AssignmentExpression(assign) = &stmt.test {
                    push(
                        assign.span,
                        Severity::Warning,
                        "Expected a conditional expression and instead saw an assignment".into(),
                        "no-cond-assign",
                    );
                }
            }
            // TypeScript reports undefined names itself, and ambient globals
            // often come from `.d.ts` files this single-file pass can't see.
            AstKind::IdentifierReference(ident) if !typescript => {
                if semantic.is_unresolved_reference(node.id())
                    && !is_known_global(&ident.name)
                    && !matches!(nodes.parent_kind(node.id()), AstKind::UnaryExpression(u) if u.operator == UnaryOperator::Typeof)
                {
                    push(
                        ident.span,
                        Severity::Warning,
                        format!("'{}' is not defined", ident.name),
                        "no-undef",
                    );
                }
            }
            _ => {}
        }
    }

    let root_scope = scoping.root_scope_id();
    for symbol in scoping.symbol_ids() {
        let flags = scoping.symbol_flags(symbol);
        let name = scoping.symbol_name(symbol);
        let span = scoping.symbol_span(symbol);
        let references = scoping.get_resolved_references(symbol);

        if flags.contains(SymbolFlags::ConstVariable) {
            for r in scoping.get_resolved_references(symbol) {
                if r.is_write() {
                    push(
                        semantic.reference_span(r),
                        Severity::Error,
                        format!("'{name}' is constant and cannot be reassigned"),
                        "no-const-assign",
                    );
                }
            }
        }

        if name.starts_with('_') || flags.intersects(SymbolFlags::CatchVariable | SymbolFlags::Ambient) {
            continue;
        }
        if !flags.intersects(SymbolFlags::Variable | SymbolFlags::Function | SymbolFlags::Class | SymbolFlags::Import) {
            continue;
        }
        if context == Context::InlineScript && scoping.symbol_scope_id(symbol) == root_scope {
            continue;
        }
        let mut references = references;
        // A use in a type annotation counts (an import used only as a type).
        if references.any(|r| r.is_read() || r.is_type()) {
            continue;
        }
        let decl = scoping.symbol_declaration(symbol);
        if matches!(
            nodes.kind(decl),
            AstKind::FormalParameter(_) | AstKind::FormalParameterRest(_) | AstKind::CatchParameter(_)
        ) {
            continue;
        }
        if is_exported(&semantic, decl) {
            continue;
        }
        // A named function expression's own name is only for recursion/stack traces.
        if let AstKind::Function(f) = nodes.kind(decl)
            && f.is_expression()
        {
            continue;
        }
        push(
            span,
            Severity::Warning,
            format!("'{name}' is declared but never used"),
            "no-unused-vars",
        );
    }

    out
}

fn is_exported(semantic: &oxc_semantic::Semantic, decl: oxc_semantic::NodeId) -> bool {
    let nodes = semantic.nodes();
    let mut id = decl;
    loop {
        let parent = nodes.parent_id(id);
        if parent == id {
            return false;
        }
        match nodes.kind(parent) {
            AstKind::ExportNamedDeclaration(_) | AstKind::ExportDefaultDeclaration(_) => {
                return true;
            }
            AstKind::VariableDeclarator(_)
            | AstKind::VariableDeclaration(_)
            | AstKind::Function(_)
            | AstKind::Class(_)
            | AstKind::BindingIdentifier(_)
            | AstKind::ObjectPattern(_)
            | AstKind::ArrayPattern(_)
            | AstKind::BindingProperty(_)
            | AstKind::AssignmentPattern(_) => id = parent,
            _ => return false,
        }
    }
}

fn operator_span(source: &str, from: u32, to: u32, op: &str) -> Option<Span> {
    let slice = source.get(from as usize..to as usize)?;
    let ix = slice.find(op)? as u32;
    Some(Span::new(from + ix, from + ix + op.len() as u32))
}

fn diagnostic_span(d: &OxcDiagnostic) -> Span {
    d.labels
        .as_slice()
        .iter()
        .find(|l| l.primary())
        .or_else(|| d.labels.as_slice().first())
        .map(|l| Span::new(l.offset(), l.offset() + l.len()))
        .unwrap_or_default()
}

fn clean_message(d: &OxcDiagnostic) -> String {
    let mut message = d.message.to_string();
    if let Some(help) = d.help.as_deref() {
        message.push_str(" — ");
        message.push_str(help);
    }
    message
}

fn is_known_global(name: &str) -> bool {
    static SET: std::sync::LazyLock<HashSet<&'static str>> =
        std::sync::LazyLock::new(|| GLOBALS.iter().copied().collect());
    SET.contains(name)
}

/// ECMAScript, browser and common Node globals.
static GLOBALS: &[&str] = &[
    "AbortController", "AbortSignal", "AggregateError", "Array", "ArrayBuffer", "Atomics",
    "Audio", "BigInt", "BigInt64Array", "BigUint64Array", "Blob", "Boolean", "BroadcastChannel",
    "Buffer", "CSS", "CanvasRenderingContext2D", "ClipboardItem", "CustomEvent", "DOMException",
    "DOMParser", "DataView", "Date", "Document", "DocumentFragment", "Element", "Error",
    "EvalError", "Event", "EventSource", "EventTarget", "File", "FileList", "FileReader",
    "FinalizationRegistry", "Float32Array", "Float64Array", "FormData", "Function", "Headers",
    "HTMLCanvasElement", "HTMLElement", "HTMLInputElement", "History", "IDBKeyRange", "Image",
    "ImageData", "Infinity", "Int16Array", "Int32Array", "Int8Array", "IntersectionObserver",
    "Intl", "JSON", "KeyboardEvent", "Location", "Map", "Math", "MessageChannel",
    "MouseEvent", "MutationObserver", "NaN", "Navigator", "Node", "NodeList", "Notification",
    "Number", "Object", "OffscreenCanvas", "Option", "Path2D", "Performance",
    "PerformanceObserver", "PointerEvent", "Promise", "Proxy", "RangeError", "ReadableStream",
    "ReferenceError", "Reflect", "RegExp", "Request", "ResizeObserver", "Response", "Set",
    "SharedArrayBuffer", "SharedWorker", "Storage", "String", "Symbol", "SyntaxError",
    "Text", "TextDecoder", "TextEncoder", "TouchEvent", "TransformStream", "TypeError",
    "URIError", "URL", "URLSearchParams", "Uint16Array", "Uint32Array", "Uint8Array",
    "Uint8ClampedArray", "WeakMap", "WeakRef", "WeakSet", "WebAssembly", "WebSocket",
    "Window", "Worker", "WritableStream", "XMLHttpRequest", "__dirname", "__filename",
    "alert", "arguments", "atob", "blur", "btoa", "caches", "cancelAnimationFrame",
    "cancelIdleCallback", "clearInterval", "clearTimeout", "close", "confirm", "console",
    "createImageBitmap", "crypto", "customElements", "decodeURI", "decodeURIComponent",
    "devicePixelRatio", "document", "encodeURI", "encodeURIComponent", "escape", "eval",
    "exports", "fetch", "focus", "frames", "getComputedStyle", "getSelection", "global",
    "globalThis", "history", "indexedDB", "innerHeight", "innerWidth", "isFinite", "isNaN",
    "localStorage", "location", "matchMedia", "module", "name", "navigator", "open",
    "opener", "outerHeight", "outerWidth", "pageXOffset", "pageYOffset", "parent",
    "parseFloat", "parseInt", "performance", "postMessage", "print", "process", "prompt",
    "queueMicrotask", "removeEventListener", "requestAnimationFrame", "requestIdleCallback",
    "require", "screen", "scroll", "scrollBy", "scrollTo", "scrollX", "scrollY", "self",
    "sessionStorage", "setInterval", "setTimeout", "structuredClone", "top", "undefined",
    "unescape", "window",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(src: &str) -> Vec<String> {
        lint(src, 0, Context::File, Dialect::JavaScript).into_iter().map(|d| d.rule.into_owned()).collect()
    }

    fn ts_rules(src: &str, dialect: Dialect) -> Vec<String> {
        lint(src, 0, Context::File, dialect).into_iter().map(|d| d.rule.into_owned()).collect()
    }

    #[test]
    fn lints_typescript() {
        let ts = |src| ts_rules(src, Dialect::TypeScript);
        assert!(ts("let x: number = ;").contains(&"syntax".to_string()));
        assert!(ts("function f() { const unused: string = 'a'; } f();").contains(&"no-unused-vars".to_string()));
        assert!(ts("const a = 1; if (a == 2) {}").contains(&"eqeqeq".to_string()));
        // Type-only uses, ambient declarations and names TypeScript resolves.
        assert!(ts("import { Foo } from './foo';\nexport let x: Foo;").is_empty(), "{:?}", ts("import { Foo } from './foo';\nexport let x: Foo;"));
        assert!(ts("declare const API_URL: string;\nexport interface P { x: number }").is_empty());
        assert!(ts("export const t: Record<string, NodeJS.Timeout> = {}; undeclaredGlobal();").is_empty());
        assert!(ts_rules("const a = <div className=\"x\" />; export default a;", Dialect::Tsx).is_empty());
        assert!(ts_rules("declare function f(a: string): void;\nexport {};", Dialect::Declaration).is_empty());
        assert!(ts_rules("declare function f(a: string: void;", Dialect::Declaration).contains(&"syntax".to_string()));
    }

    #[test]
    fn detects_common_problems() {
        assert!(rules("let x = ;").contains(&"syntax".to_string()));
        assert!(rules("const a = 1; a = 2; console.log(a);").contains(&"no-const-assign".to_string()));
        assert!(rules("function f() { const unused = 1; } f();").contains(&"no-unused-vars".to_string()));
        assert!(rules("if (a == 1) {}").contains(&"eqeqeq".to_string()));
        assert!(!rules("let a = 1; if (a == null) {}").contains(&"eqeqeq".to_string()));
        assert!(rules("debugger;").contains(&"no-debugger".to_string()));
        assert!(rules("foo();").contains(&"no-undef".to_string()));
        assert!(!rules("console.log(window);").contains(&"no-undef".to_string()));
        assert!(rules("const o = { a: 1, a: 2 }; console.log(o);").contains(&"no-dupe-keys".to_string()));
        assert!(rules("var x = 1; console.log(x);").contains(&"no-var".to_string()));
        assert!(rules("let x = 1; if (x === NaN) {}").contains(&"use-isnan".to_string()));
        assert!(!rules("export const x = 1;").contains(&"no-unused-vars".to_string()));
        assert!(!rules("function f(a) {} f();").contains(&"no-unused-vars".to_string()));
    }
}
