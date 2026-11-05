//! Bounded, syntax-aware Git patch previews for the attachment file picker.
//!
//! Like lazygitrs, highlight *source* with Tree-sitter and style the surrounding
//! Git diff structure separately. Only the preview's short text sample is parsed,
//! and no parsing happens in the terminal's drawing or smooth-scroll loop.

#[cfg(feature = "patch-syntax")]
use std::path::Path;
#[cfg(feature = "patch-syntax")]
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchTone {
    Default,
    Meta,
    Hunk,
    Add,
    Remove,
    Keyword,
    String,
    Comment,
    Number,
    Function,
    Type,
}

#[derive(Debug, Clone)]
pub struct PatchFragment {
    pub text: String,
    pub tone: PatchTone,
}

#[derive(Debug, Clone)]
pub struct PatchPreview {
    pub lines: Vec<Vec<PatchFragment>>,
    pub truncated: bool,
}

#[cfg(feature = "patch-syntax")]
#[derive(Debug, Clone, Copy)]
enum SourceLanguage { Rust, C }

#[cfg(feature = "patch-syntax")]
fn language_for_file(name: &str) -> Option<SourceLanguage> {
    match Path::new(name).extension().and_then(|ext| ext.to_str())? {
        "rs" => Some(SourceLanguage::Rust),
        "c" | "h" => Some(SourceLanguage::C),
        _ => None,
    }
}

#[cfg(feature = "patch-syntax")]
fn syntax_tone(kind: &str, parent_kind: &str) -> Option<PatchTone> {
    if kind.contains("comment") { return Some(PatchTone::Comment); }
    if kind.contains("string") || kind.contains("char_literal") { return Some(PatchTone::String); }
    if kind.contains("number") || kind.contains("integer") || kind.contains("float") { return Some(PatchTone::Number); }
    if kind.contains("type_identifier") || kind == "primitive_type" { return Some(PatchTone::Type); }
    if kind == "identifier" && matches!(parent_kind,
        "call_expression" | "function_declarator" | "function_item") {
        return Some(PatchTone::Function);
    }
    if matches!(kind,
        "fn" | "let" | "mut" | "pub" | "impl" | "struct" | "enum" | "trait" | "use"
        | "mod" | "const" | "static" | "return" | "if" | "else" | "while" | "for"
        | "loop" | "match" | "break" | "continue" | "async" | "await" | "unsafe"
        | "typedef" | "int" | "char" | "void" | "long" | "short" | "unsigned"
        | "signed" | "sizeof" | "switch" | "case" | "default" | "do" | "goto"
        | "extern" | "volatile" | "register" | "inline" | "union" | "auto"
        | "#include" | "#define") {
        return Some(PatchTone::Keyword);
    }
    None
}

/// Map byte ranges from one reconstituted source buffer to the patch rows from
/// which they came. `+` lines and context lines are new-source text; `-` lines
/// are old-source text and retain their diff color for clarity.
#[cfg(feature = "patch-syntax")]
fn source_highlights(
    lines: &[&str], row_start: usize, language: SourceLanguage,
    out: &mut [Vec<(usize, usize, PatchTone)>],
) {
    let mut source = String::new();
    let mut positions: Vec<(usize, usize, usize)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") { continue; }
        if let Some(code) = line.strip_prefix('+').or_else(|| line.strip_prefix(' ')) {
            let start = source.len();
            source.push_str(code);
            let end = source.len();
            source.push('\n');
            positions.push((row_start + i, start, end));
        }
    }
    if source.is_empty() { return; }
    let mut parser = Parser::new();
    let language = match language {
        SourceLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
        SourceLanguage::C => tree_sitter_c::LANGUAGE.into(),
    };
    if parser.set_language(&language).is_err() { return; }
    let Some(tree) = parser.parse(source.as_bytes(), None) else { return; };
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let parent = node.parent().map_or("", |parent| parent.kind());
        if let Some(tone) = syntax_tone(node.kind(), parent) {
            record_range(node, tone, &positions, out);
            continue;
        }
        // Descend until we find syntactic leaf tokens. We do not need an
        // additional query engine or an entire repository-sized parse.
        for child in (0..node.child_count()).rev() {
            if let Some(child) = node.child(child) { stack.push(child); }
        }
    }
}

#[cfg(feature = "patch-syntax")]
fn record_range(
    node: Node<'_>, tone: PatchTone,
    positions: &[(usize, usize, usize)],
    out: &mut [Vec<(usize, usize, PatchTone)>],
) {
    for &(row, start, end) in positions {
        let from = node.start_byte().max(start);
        let to = node.end_byte().min(end);
        if from < to {
            // Patch line begins with its Git prefix, hence +1.
            if let Some(tokens) = out.get_mut(row) {
                tokens.push((from - start + 1, to - start + 1, tone));
            }
        }
    }
}

fn build_line(line: &str, tokens: &[(usize, usize, PatchTone)]) -> Vec<PatchFragment> {
    let base = if line.starts_with("diff --git ") || line.starts_with("index ")
        || line.starts_with("--- ") || line.starts_with("+++ ")
        || line.starts_with("new file mode") || line.starts_with("deleted file mode") {
        PatchTone::Meta
    } else if line.starts_with("@@") || line.starts_with("\\ No newline") {
        PatchTone::Hunk
    } else if line.starts_with('+') { PatchTone::Add }
    else if line.starts_with('-') { PatchTone::Remove }
    else { PatchTone::Default };
    if tokens.is_empty() || matches!(base, PatchTone::Meta | PatchTone::Hunk | PatchTone::Remove) {
        return vec![PatchFragment { text: line.to_owned(), tone: base }];
    }
    let mut colored = Vec::new();
    let mut sorted = tokens.to_vec();
    sorted.sort_by_key(|(start, end, _)| (*start, *end));
    let mut offset = 0usize;
    for (from, to, tone) in sorted {
        let from = from.max(offset).min(line.len());
        let to = to.max(from).min(line.len());
        if !line.is_char_boundary(from) || !line.is_char_boundary(to) { continue; }
        if from > offset {
            colored.push(PatchFragment { text: line[offset..from].to_owned(), tone: base });
        }
        if to > from {
            colored.push(PatchFragment { text: line[from..to].to_owned(), tone });
        }
        offset = to;
    }
    if offset < line.len() {
        colored.push(PatchFragment { text: line[offset..].to_owned(), tone: base });
    }
    colored
}

pub fn highlight_patch(text: &str, truncated: bool) -> PatchPreview {
    // The file picker has already bounded text reads to 24 KiB and 180 rows.
    let lines: Vec<_> = text.lines().take(180).collect();
    #[cfg(feature = "patch-syntax")]
    let mut spans: Vec<Vec<(usize, usize, PatchTone)>> = vec![vec![]; lines.len()];
    #[cfg(feature = "patch-syntax")]
    {
        let mut section_start: Option<(usize, SourceLanguage)> = None;
        for (index, line) in lines.iter().enumerate() {
            if let Some(filename) = line.strip_prefix("diff --git ") {
                if let Some((start, language)) = section_start.take() {
                    source_highlights(&lines[start..index], start, language, &mut spans);
                }
                let filename = filename.rsplit_once(" b/").map_or(filename, |(_, file)| file);
                section_start = language_for_file(filename).map(|lang| (index, lang));
            }
        }
        if let Some((start, language)) = section_start {
            source_highlights(&lines[start..], start, language, &mut spans);
        }
    }
    PatchPreview {
        lines: lines.iter().enumerate().map(|(i, line)| {
            #[cfg(feature = "patch-syntax")]
            { build_line(line, &spans[i]) }
            #[cfg(not(feature = "patch-syntax"))]
            { let _ = i; build_line(line, &[]) }
        }).collect(),
        truncated: truncated || text.lines().count() > 180,
    }
}

#[cfg(test)]
mod tests {
    use super::{highlight_patch, PatchTone};

    #[cfg(feature = "patch-syntax")]
    #[test]
    fn syntax_highlighting_keeps_git_diff_structure_and_source_intact() {
        let text = "diff --git a/src/main.rs b/src/main.rs\nindex 001..002 100644\n@@ -1 +1 @@\n-fn main() {}\n+fn main() { let answer = 42; }\n";
        let preview = highlight_patch(text, false);
        assert_eq!(preview.lines.len(), 5);
        assert_eq!(preview.lines[2][0].tone, PatchTone::Hunk);
        assert_eq!(preview.lines[3][0].tone, PatchTone::Remove);
        assert!(preview.lines[4].iter().any(|frag| frag.tone == PatchTone::Keyword));
        assert_eq!(preview.lines[4].iter().map(|frag| frag.text.as_str()).collect::<String>(), "+fn main() { let answer = 42; }");
    }
    #[cfg(not(feature = "patch-syntax"))]
    #[test]
    fn minimal_build_keeps_diff_structure_without_syntax_parsing() {
        let text = "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let preview = highlight_patch(text, false);
        assert_eq!(preview.lines.len(), 4);
        assert_eq!(preview.lines[1][0].tone, PatchTone::Hunk);
        assert_eq!(preview.lines[2][0].tone, PatchTone::Remove);
        assert_eq!(preview.lines[3][0].tone, PatchTone::Add);
    }

    #[test]
    fn no_grammar_still_highlights_additions_and_deletions() {
        let text = "diff --git a/s.txt b/s.txt\n-old\n+new";
        let preview = highlight_patch(text, false);
        assert_eq!(preview.lines[1][0].tone, PatchTone::Remove);
        assert_eq!(preview.lines[2][0].tone, PatchTone::Add);
    }
}
