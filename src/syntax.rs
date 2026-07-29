use tree_sitter::{Language, Parser};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "function",
    "function.method",
    "keyword",
    "keyword.operator",
    "module",
    "number",
    "property",
    "property.definition",
    "string",
    "string.escape",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyntaxKind {
    Plain,
    Comment,
    String,
    Keyword,
    Number,
    Type,
    Function,
    Attribute,
    Property,
}

pub struct SyntaxSpan {
    pub text: String,
    pub kind: SyntaxKind,
}

/// Highlights languages whose grammar ships with a Tree-sitter query. Unknown
/// languages deliberately return `None` so the renderer retains its compact
/// fallback highlighter instead of dropping all colour.
pub fn highlight(source: &str, language: &str) -> Option<Vec<SyntaxSpan>> {
    let (language, name, highlights, injections, locals) = configuration_parts(language)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    if parser.parse(source, None)?.root_node().has_error() {
        return None;
    }
    let highlights = if name == "c_sharp" {
        format!("{highlights}\n(property_declaration name: (identifier) @property)")
    } else {
        highlights.to_owned()
    };
    let mut config =
        HighlightConfiguration::new(language, name, &highlights, injections, locals).ok()?;
    config.configure(HIGHLIGHT_NAMES);

    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(&config, source.as_bytes(), None, |_| None)
        .ok()?;
    let mut current = Vec::new();
    let mut result = Vec::new();

    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(highlight) => current.push(highlight.0),
            HighlightEvent::HighlightEnd => {
                current.pop();
            }
            HighlightEvent::Source { start, end } => push_span(
                &mut result,
                &source[start..end],
                current
                    .last()
                    .and_then(|index| HIGHLIGHT_NAMES.get(*index))
                    .map_or(SyntaxKind::Plain, |name| syntax_kind(name)),
            ),
        }
    }

    Some(result)
}

fn configuration_parts(
    language: &str,
) -> Option<(
    Language,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
)> {
    match language {
        "rs" | "rust" => Some((
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY,
            "",
        )),
        "cs" | "csharp" | "c#" => Some((
            tree_sitter_c_sharp::LANGUAGE.into(),
            "c_sharp",
            tree_sitter_c_sharp::HIGHLIGHTS_QUERY,
            "",
            "",
        )),
        "ts" | "typescript" => Some((
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
            tree_sitter_typescript::LOCALS_QUERY,
        )),
        "tsx" => Some((
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "tsx",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
            tree_sitter_typescript::LOCALS_QUERY,
        )),
        "js" | "jsx" | "javascript" | "mjs" | "cjs" => Some((
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::INJECTIONS_QUERY,
            tree_sitter_javascript::LOCALS_QUERY,
        )),
        "py" | "python" => Some((
            tree_sitter_python::LANGUAGE.into(),
            "python",
            tree_sitter_python::HIGHLIGHTS_QUERY,
            "",
            "",
        )),
        "json" | "jsonc" => Some((
            tree_sitter_json::LANGUAGE.into(),
            "json",
            tree_sitter_json::HIGHLIGHTS_QUERY,
            "",
            "",
        )),
        "sh" | "bash" | "zsh" | "shell" => Some((
            tree_sitter_bash::LANGUAGE.into(),
            "bash",
            tree_sitter_bash::HIGHLIGHT_QUERY,
            "",
            "",
        )),
        _ => None,
    }
}

fn syntax_kind(name: &str) -> SyntaxKind {
    match name {
        "comment" => SyntaxKind::Comment,
        "string" | "string.escape" => SyntaxKind::String,
        "constant" | "constant.builtin" | "number" => SyntaxKind::Number,
        "keyword" | "keyword.operator" | "tag" => SyntaxKind::Keyword,
        "type" | "type.builtin" | "module" => SyntaxKind::Type,
        "constructor" | "function" | "function.method" => SyntaxKind::Function,
        "attribute" => SyntaxKind::Attribute,
        "property" | "property.definition" => SyntaxKind::Property,
        _ => SyntaxKind::Plain,
    }
}

fn push_span(spans: &mut Vec<SyntaxSpan>, text: &str, kind: SyntaxKind) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.kind == kind {
            last.text.push_str(text);
            return;
        }
    }
    spans.push(SyntaxSpan {
        text: text.to_owned(),
        kind,
    });
}

#[cfg(test)]
mod tests {
    use super::{SyntaxKind, highlight};

    #[test]
    fn csharp_keeps_attributes_properties_and_types_distinct() {
        let spans = highlight(
            "[Obsolete] public sealed class Job { string Name { get; set; } }",
            "csharp",
        )
        .expect("C# grammar");
        assert!(
            spans
                .iter()
                .any(|span| span.text == "Obsolete" && span.kind == SyntaxKind::Attribute)
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text == "Job" && span.kind == SyntaxKind::Type)
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text == "Name" && span.kind == SyntaxKind::Property)
        );
    }

    #[test]
    fn javascript_does_not_treat_property_as_a_type() {
        let spans = highlight(
            "const app = { title: \"Devez\" }; app.title();",
            "javascript",
        )
        .expect("JavaScript grammar");
        assert!(
            spans
                .iter()
                .any(|span| span.text == "const" && span.kind == SyntaxKind::Keyword)
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text == "\"Devez\"" && span.kind == SyntaxKind::String)
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text == "title" && span.kind == SyntaxKind::Property)
        );
    }
}
