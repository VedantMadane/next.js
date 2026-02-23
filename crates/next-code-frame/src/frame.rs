use std::{fmt::Write, ops::Range};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::{
    highlight::{ColorScheme, Language, Lines, apply_line_highlights, extract_highlights},
    terminal::get_terminal_width,
};

/// A source location with line and column
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    /// 1-indexed line number
    pub line: usize,
    /// 1-indexed column number as byte offset
    #[serde(default)]
    pub column: Option<usize>,
}

/// Location information for the error in the source code.
///
/// Columns are 1-indexed byte offsets, half-open: `[start.column, end.column)`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeFrameLocation {
    /// Starting location
    pub start: Location,
    /// Optional ending location (line inclusive, column half-open)
    pub end: Option<Location>,
}

/// Options for rendering the code frame
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CodeFrameOptions {
    /// Number of lines to show before the error
    pub lines_above: usize,
    /// Number of lines to show after the error
    pub lines_below: usize,
    /// Whether to use ANSI color output
    pub color: bool,
    /// Whether to attempt syntax highlighting
    pub highlight_code: bool,
    /// Optional message to display with the error
    pub message: Option<String>,
    /// Maximum width for the output (None = terminal width)
    pub max_width: Option<usize>,
    /// Language hint for keyword highlighting
    #[serde(default)]
    pub language: Language,
}

impl Default for CodeFrameOptions {
    fn default() -> Self {
        Self {
            lines_above: 2,
            lines_below: 3,
            color: false,
            highlight_code: false,
            message: None,
            max_width: None,
            language: Language::default(),
        }
    }
}

/// Result of applying line truncation.
/// All offsets are in byte space.
struct TruncationResult {
    /// The visible content after truncation (may include "..." prefix/suffix)
    visible_content: String,
    /// The byte offset in the original line where visible source content starts
    byte_offset: usize,
    /// The byte length of any prefix prepended before source content (e.g., "..." = 3)
    prefix_len: usize,
}

/// Convert a source-column range to display coordinates, accounting for
/// line truncation and available width.
///
/// Returns `(display_col, display_length)` where `display_col` is the
/// number of leading spaces before the `^` markers.
fn marker_display_position(
    col_start: usize,
    col_end: usize,
    truncation_offset: usize,
    available_width: usize,
) -> (usize, usize) {
    // Map source column to display column, accounting for "..." prefix
    let display_col = if truncation_offset > 0 {
        if col_start <= truncation_offset {
            ELLIPSIS_DISPLAY_OFFSET
        } else {
            (col_start - truncation_offset) + ELLIPSIS_DISPLAY_OFFSET
        }
    } else {
        col_start.max(1)
    };

    // Marker length: at least 1 caret, clamped to available width
    let length = col_end
        .saturating_sub(col_start)
        .max(1)
        .min(available_width.saturating_sub(display_col.saturating_sub(1)));

    (display_col, length)
}

/// Renders a code frame showing the location of an error in source code
pub fn render_code_frame(
    source: &str,
    location: &CodeFrameLocation,
    options: &CodeFrameOptions,
) -> Result<String> {
    if source.is_empty() {
        return Ok(String::new());
    }

    // Single O(n) scan to compute line-start offsets. This replaces both the
    // Vec<&str> allocation that split('\n').collect() would produce and the
    // duplicate scan inside extract_highlights.
    let lines = Lines::new(source);

    // Validate location (convert 1-indexed to 0-indexed)
    let start_line_idx = location.start.line.saturating_sub(1);
    if start_line_idx >= lines.len() {
        return Ok(String::new());
    }

    let end_line_idx = location
        .end
        .map(|l| l.line.saturating_sub(1).min(lines.len() - 1))
        .unwrap_or(start_line_idx);
    if end_line_idx < start_line_idx {
        bail!("source location invalid end_line {end_line_idx} < {start_line_idx}");
    }

    // Extract the start column (None means no column highlighting)
    let start_column = location.start.column;

    // Normalize end_column (half-open): defaults to start.column + 1 if absent
    let end_column = location
        .end
        .and_then(|l| l.column)
        .or(start_column.map(|c| c + 1));

    // Calculate window of lines to show (0-indexed, last is exclusive)
    let first_line_idx = start_line_idx.saturating_sub(options.lines_above);
    let last_line_idx = (end_line_idx + options.lines_below + 1).min(lines.len());

    let line_highlights = if options.highlight_code {
        Some(extract_highlights(
            &lines,
            first_line_idx..last_line_idx,
            options.language,
        ))
    } else {
        None
    };

    let gutter_width = last_line_idx.to_string().len();

    let max_width = options.max_width.unwrap_or_else(get_terminal_width);

    // Format: "> N | code" or "  N | code"
    // That's: 2 (marker + space) + gutter_width + SEPARATOR.len()
    let gutter_total_width = 2 + gutter_width + SEPARATOR.len();
    let available_code_width = max_width.saturating_sub(gutter_total_width);
    if available_code_width == 0 {
        bail!("max_width {max_width} too small to render a code frame")
    }

    let truncation_offset = calculate_truncation_offset(
        &lines,
        first_line_idx..last_line_idx,
        start_column.unwrap_or(0),
        end_column.unwrap_or(0),
        available_code_width,
    );

    let color_scheme = if options.color {
        ColorScheme::colored()
    } else {
        ColorScheme::plain()
    };
    let mut output = String::new();
    // Track whether we need a newline before the next section.
    // By prepending newlines instead of appending them we avoid a
    // trailing newline that callers would have to strip.
    let mut needs_newline = false;

    // Add message if provided and no column specified
    if let Some(ref message) = options.message
        && start_column.is_none()
    {
        output.extend(std::iter::repeat_n(' ', gutter_total_width));
        output.push_str(color_scheme.message);
        output.push_str(message);
        output.push_str(color_scheme.reset);
        needs_newline = true;
    }

    for line_idx in first_line_idx..last_line_idx {
        let line_content = lines.content(line_idx);
        let is_error_line = line_idx >= start_line_idx && line_idx <= end_line_idx;
        let line_num = line_idx + 1;

        // Apply consistent truncation to all lines (all offsets in bytes)
        let truncation = truncate_line(line_content, truncation_offset, available_code_width);

        let visible_content = if let Some(highlight) = line_highlights
            .as_ref()
            .and_then(|h| h.get(line_idx - first_line_idx))
        {
            apply_line_highlights(
                &truncation.visible_content,
                highlight,
                &color_scheme,
                truncation.byte_offset,
                truncation.prefix_len,
            )
        } else {
            truncation.visible_content
        };

        // Separate from previous line/section
        if needs_newline {
            output.push('\n');
        }
        needs_newline = true;

        if is_error_line {
            output.push_str(color_scheme.marker);
            output.push('>');
            output.push_str(color_scheme.reset);
        } else {
            output.push(' ');
        }
        output.push(' ');
        output.push_str(color_scheme.gutter);
        write!(output, "{:>width$} |", line_num, width = gutter_width).unwrap();
        output.push_str(color_scheme.reset);
        if !visible_content.is_empty() {
            output.push(' ');
            output.push_str(&visible_content);
        }

        // Add marker line if this is an error line with column info
        if is_error_line && let Some(start_col) = start_column {
            let end_col = end_column.unwrap_or(start_col + 1);
            let line_len = line_content.len();

            // Determine which columns to underline on this error line
            let (col_start, col_end) = if start_line_idx == end_line_idx {
                (start_col, end_col)
            } else if line_idx == start_line_idx {
                (start_col, line_len)
            } else if line_idx == end_line_idx {
                (1, end_col)
            } else {
                (1, line_len + 1) // intermediate line: underline everything
            };

            // Clamp to line bounds (1-indexed)
            let col_start = col_start.min(line_len + 1);
            let col_end = col_end.min(line_len + 2);

            // project into display space
            let (marker_col, marker_length) = marker_display_position(
                col_start,
                col_end,
                truncation.byte_offset,
                available_code_width,
            );

            output.push_str("\n  ");
            output.push_str(color_scheme.gutter);
            write!(output, "{:>width$} |", "", width = gutter_width).unwrap();

            output.push_str(color_scheme.reset);
            output.extend(std::iter::repeat_n(' ', marker_col));
            output.push_str(color_scheme.marker);
            output.extend(std::iter::repeat_n('^', marker_length));
            output.push_str(color_scheme.reset);

            if line_idx == end_line_idx
                && let Some(ref message) = options.message
            {
                output.push(' ');
                output.push_str(color_scheme.message);
                output.push_str(message);
                output.push_str(color_scheme.reset);
            }
        }
    }

    Ok(output)
}

const ELLIPSIS: &str = "...";
const SEPARATOR: &str = " | ";
/// Display offset for content after an ellipsis prefix
const ELLIPSIS_DISPLAY_OFFSET: usize = ELLIPSIS.len() + 1;

/// Calculate the truncation offset (in bytes) for all lines in the window.
/// This ensures all lines are "scrolled" to the same horizontal position, centering the error
/// range. All column values are byte offsets.
// TODO: use a display-width crate (e.g. `unicode-width`) instead of byte length
// for correct CJK / emoji column counting.
fn calculate_truncation_offset(
    lines: &Lines<'_>,
    window: Range<usize>,
    start_column: usize,
    end_column: usize,
    available_width: usize,
) -> usize {
    // Check if any line in the window needs truncation
    let needs_truncation = window
        .clone()
        .any(|i| lines.content(i).len() > available_width);

    // All lines are short enough or we don't have an error column so start at beginning
    if !needs_truncation || start_column == 0 {
        return 0;
    }

    // If we need truncation, center the error range
    // We need to account for the "..." ellipsis (3 chars) on each side
    let available_with_ellipsis = available_width.saturating_sub(2 * ELLIPSIS.len());

    // Calculate the midpoint of the error range
    // end_column is exclusive, so the range is [start_column, end_column)
    let start_0idx = start_column.saturating_sub(1);
    let end_0idx = end_column.saturating_sub(1);
    let error_midpoint = (start_0idx + end_0idx) / 2;

    // Try to center the error range in the window
    let half_width = available_with_ellipsis / 2;

    error_midpoint.saturating_sub(half_width)
}

/// Truncate a line at a specific byte offset, adding ellipsis as needed.
/// The `offset` is snapped forward to the nearest UTF-8 character boundary
/// to avoid splitting multi-byte characters.
fn truncate_line(line: &str, offset: usize, max_width: usize) -> TruncationResult {
    // If no offset and line fits, return as-is
    if offset == 0 && line.len() <= max_width {
        return TruncationResult {
            visible_content: line.to_string(),
            byte_offset: 0,
            prefix_len: 0,
        };
    }

    // Snap offset to nearest char boundary (forward)
    let byte_offset = line.ceil_char_boundary(offset);

    let mut result = String::with_capacity(max_width);

    // Add leading ellipsis if we're starting mid-line
    let prefix_len = if byte_offset > 0 {
        result.push_str(ELLIPSIS);
        ELLIPSIS.len()
    } else {
        0
    };

    // Calculate how much content we can show (in bytes, approximate)
    let available_content_width = if byte_offset > 0 {
        max_width.saturating_sub(ELLIPSIS.len())
    } else {
        max_width
    };

    // Check if offset is past line length
    let remaining_line = if byte_offset < line.len() {
        &line[byte_offset..]
    } else {
        // Offset is past line length - show just ellipsis
        return TruncationResult {
            visible_content: ELLIPSIS.to_string(),
            byte_offset,
            prefix_len: ELLIPSIS.len(),
        };
    };

    let needs_trailing_ellipsis = remaining_line.len() > available_content_width;
    let content_width = if needs_trailing_ellipsis {
        available_content_width.saturating_sub(ELLIPSIS.len())
    } else {
        available_content_width.min(remaining_line.len())
    };

    // Find the largest byte offset <= content_width that is on a char boundary
    let visible_end = remaining_line.floor_char_boundary(content_width);

    result.push_str(&remaining_line[..visible_end]);

    if needs_trailing_ellipsis {
        result.push_str(ELLIPSIS);
    }

    TruncationResult {
        visible_content: result,
        byte_offset,
        prefix_len,
    }
}
