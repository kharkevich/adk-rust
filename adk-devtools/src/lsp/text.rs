//! Positions: the model counts 1-based lines and characters, LSP counts 0-based lines and
//! UTF-16 code units.

use lsp_types::Position;

/// The text of line `line` (0-based) of `text`, without its line ending.
pub fn line_text(text: &str, line: u32) -> Option<&str> {
    text.split('\n').nth(line as usize).map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// The LSP position of 1-based `line` and `character` (counted in characters) in `text`.
///
/// A character past the end of its line is clamped to the end; a line past the end of the
/// text is `None`.
///
/// # Example
///
/// ```rust
/// use adk_devtools::lsp::text::to_position;
///
/// let text = "let ü = 1;\nlet 𝔵 = ü;\n";
/// let position = to_position(text, 2, 10).unwrap();
/// assert_eq!((position.line, position.character), (1, 10)); // 𝔵 is two UTF-16 units
/// ```
pub fn to_position(text: &str, line: u32, character: u32) -> Option<Position> {
    let zero_line = line.checked_sub(1)?;
    let content = line_text(text, zero_line)?;
    let wanted = character.saturating_sub(1) as usize;
    let utf16: usize = content.chars().take(wanted).map(char::len_utf16).sum();
    Some(Position::new(zero_line, u32::try_from(utf16).unwrap_or(u32::MAX)))
}

/// The 1-based character column of `position` in `line`, a line's text.
///
/// # Example
///
/// ```rust
/// use adk_devtools::lsp::text::char_column;
/// use lsp_types::Position;
///
/// assert_eq!(char_column("let 𝔵 = 1;", Position::new(0, 6)), 6);
/// ```
pub fn char_column(line: &str, position: Position) -> u32 {
    let mut units = 0usize;
    let mut chars = 0u32;
    for ch in line.chars() {
        if units >= position.character as usize {
            break;
        }
        units += ch.len_utf16();
        chars += 1;
    }
    chars + 1
}

/// The byte offset of `position` in `text`, clamped to the end of its line and of the text.
pub fn byte_offset(text: &str, position: Position) -> usize {
    let mut offset = 0usize;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if index == position.line as usize {
            let content = line.strip_suffix('\n').unwrap_or(line);
            let content = content.strip_suffix('\r').unwrap_or(content);
            let mut units = 0usize;
            for (byte, ch) in content.char_indices() {
                if units >= position.character as usize {
                    return offset + byte;
                }
                units += ch.len_utf16();
            }
            return offset + content.len();
        }
        offset += line.len();
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_utf16_units() {
        let text = "a𝔵b\r\nü\n";
        assert_eq!(to_position(text, 1, 3), Some(Position::new(0, 3)));
        assert_eq!(to_position(text, 1, 99), Some(Position::new(0, 4)));
        assert_eq!(to_position(text, 2, 2), Some(Position::new(1, 1)));
        assert_eq!(to_position(text, 9, 1), None);
        assert_eq!(to_position(text, 0, 1), None);
        assert_eq!(char_column("a𝔵b", Position::new(0, 3)), 3);
        assert_eq!(byte_offset(text, Position::new(0, 3)), 5);
        assert_eq!(byte_offset(text, Position::new(0, 9)), 6, "clamped before the CR");
        assert_eq!(byte_offset(text, Position::new(1, 1)), 10);
        assert_eq!(byte_offset(text, Position::new(5, 0)), text.len());
        assert_eq!(line_text(text, 0), Some("a𝔵b"));
    }
}
