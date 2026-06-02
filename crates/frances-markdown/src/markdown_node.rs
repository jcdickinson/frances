//! Our own markdown AST — mirrors the mdast structure without leaking
//! third-party types into the public API.
//!
//! Block-level variants are produced one-per-root-child during the
//! mdast → MarkdownNode conversion (step 3). Inline variants appear as
//! children inside `Paragraph` and `Heading` nodes.
//!
//! Rendering (step 4) matches on the discriminant and controls the full
//! interior layout of a `MarkdownBlock`.

/// A single node in our markdown tree.
///
/// Each variant carries exactly the data its mdast counterpart provides —
/// children (as `Vec<MarkdownNode>`), string values, and scalar metadata
/// like heading depth or list ordering. No hybrid fields.
#[derive(Debug, Clone, PartialEq)]
pub enum MarkdownNode {
    // ── Block-level ──────────────────────────────────────────────

    /// A paragraph of inline content.
    Paragraph { children: Vec<MarkdownNode> },

    /// A heading (depth 1–6).
    Heading {
        depth: u8,
        children: Vec<MarkdownNode>,
    },

    /// A fenced or indented code block.
    Code {
        /// Language tag (e.g. `"rust"`), if present.
        lang: Option<String>,
        /// The raw code body.
        value: String,
    },

    /// Raw HTML — rendered as a code block.
    Html { value: String },

    /// A block quote.
    Blockquote { children: Vec<MarkdownNode> },

    /// A list (ordered or unordered).
    List {
        ordered: bool,
        start: Option<u32>,
        children: Vec<MarkdownNode>,
    },

    /// A single list item. Used as a child of `List`.
    ListItem { children: Vec<MarkdownNode> },

    /// A horizontal rule (`---`, `***`, `___`).
    ThematicBreak,

    // ── Inline ──────────────────────────────────────────────────

    /// Plain text.
    Text { value: String },

    /// **Strong** (bold) text.
    Strong { children: Vec<MarkdownNode> },

    /// *Emphasis* (italic) text.
    Emphasis { children: Vec<MarkdownNode> },

    /// `Inline code` span.
    InlineCode { value: String },

    /// `[text](url)`.
    Link {
        url: String,
        title: Option<String>,
        children: Vec<MarkdownNode>,
    },

    /// `![alt](url)`.
    Image {
        url: String,
        alt: String,
        title: Option<String>,
    },

    /// A hard line break (`\` or two trailing spaces).
    Break,
}

