//! Shared test utilities for TUI snapshot tests.

use ratatui::buffer::Buffer;

/// Convert a ratatui Buffer to a string, trimming trailing whitespace per line.
pub fn buffer_to_string(buffer: &Buffer) -> String {
    let mut output = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            output.push(
                buffer
                    .cell((x, y))
                    .unwrap()
                    .symbol()
                    .chars()
                    .next()
                    .unwrap_or(' '),
            );
        }
        output.push('\n');
    }
    output
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}
