use std::fmt::Write as _;
use std::io::Write;

use base64::Engine;

use super::PlanImageError;

const KITTY_CHUNK_SIZE: usize = 4096;

/// Terminal graphics protocol used for an inline plan image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineImageProtocol {
    Kitty,
    Iterm2,
}

/// Terminal dimensions relevant to inline-image scaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub columns: u16,
    pub pixel_width: Option<u32>,
}

/// A validated inline-image request and its optional terminal dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineImageRequest {
    pub protocol: InlineImageProtocol,
    pub terminal: Option<TerminalSize>,
}

/// A rendered PNG already wrapped in the selected terminal protocol.
///
/// The private bytes ensure callers cannot accidentally write a raw PNG to
/// the terminal after requesting an inline image.
#[derive(Debug)]
pub struct RenderedInlineImage {
    bytes: Vec<u8>,
}

/// Detect the terminal graphics protocol without reading process-global state.
///
/// Passing environment lookup in keeps the precedence and unsupported cases
/// deterministic in tests. tmux is checked before its underlying terminal so
/// inherited Kitty/iTerm variables cannot accidentally bypass the guard.
pub fn detect_inline_image_protocol<F>(
    mut env: F,
    stdout_is_terminal: bool,
) -> Result<InlineImageProtocol, PlanImageError>
where
    F: FnMut(&str) -> Option<String>,
{
    if !stdout_is_terminal {
        return Err(PlanImageError::InlineImageStdoutNotTerminal);
    }

    let term_program = env("TERM_PROGRAM");
    let tmux = env("TMUX");
    if term_program.as_deref() == Some("tmux") || tmux.is_some() {
        return Err(PlanImageError::InlineImageTmuxUnsupported);
    }

    let term = env("TERM");
    let kitty_window_id = env("KITTY_WINDOW_ID");
    if term_program.as_deref() == Some("ghostty")
        || term.as_deref() == Some("xterm-kitty")
        || kitty_window_id.is_some()
    {
        return Ok(InlineImageProtocol::Kitty);
    }

    let lc_terminal = env("LC_TERMINAL");
    if matches!(term_program.as_deref(), Some("iTerm.app" | "WezTerm"))
        || lc_terminal.as_deref() == Some("iTerm2")
    {
        return Ok(InlineImageProtocol::Iterm2);
    }

    Err(PlanImageError::InlineImageUnsupportedTerminal)
}

/// Best-effort terminal dimensions for Kitty's column scaling hint.
pub fn terminal_size() -> Option<TerminalSize> {
    let size = crossterm::terminal::window_size().ok()?;
    (size.columns > 0).then_some(TerminalSize {
        columns: size.columns,
        pixel_width: (size.width > 0).then_some(u32::from(size.width)),
    })
}

pub(crate) fn encode_inline_image(
    request: InlineImageRequest,
    png: &[u8],
    png_width: u32,
) -> RenderedInlineImage {
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    let bytes = match request.protocol {
        InlineImageProtocol::Kitty => {
            encode_kitty(&encoded, png_width, request.terminal).into_bytes()
        }
        InlineImageProtocol::Iterm2 => encode_iterm2(&encoded, png.len()).into_bytes(),
    };
    RenderedInlineImage { bytes }
}

fn encode_kitty(encoded: &str, png_width: u32, terminal: Option<TerminalSize>) -> String {
    let mut output = String::with_capacity(encoded.len() + 128);
    let chunk_count = encoded.len().div_ceil(KITTY_CHUNK_SIZE);
    debug_assert!(chunk_count > 0, "a rendered PNG cannot be empty");

    for (index, chunk) in encoded.as_bytes().chunks(KITTY_CHUNK_SIZE).enumerate() {
        let more = u8::from(index + 1 < chunk_count);
        if index == 0 {
            output.push_str("\x1b_Ga=T,f=100");
            if let Some(terminal) = terminal
                && terminal
                    .pixel_width
                    .is_none_or(|pixel_width| png_width > pixel_width)
            {
                write!(output, ",c={}", terminal.columns).expect("writing to a String cannot fail");
            }
            write!(output, ",m={more};").expect("writing to a String cannot fail");
        } else {
            write!(output, "\x1b_Gm={more};").expect("writing to a String cannot fail");
        }
        output.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        output.push_str("\x1b\\");
    }
    output.push('\n');
    output
}

fn encode_iterm2(encoded: &str, png_size: usize) -> String {
    format!(
        "\x1b]1337;File=inline=1;size={png_size};width=100%;preserveAspectRatio=1:{encoded}\x07\n"
    )
}

/// Write a fully encoded inline image and flush it immediately.
pub fn write_inline_image<W: Write>(
    image: RenderedInlineImage,
    mut output: W,
) -> Result<(), PlanImageError> {
    output
        .write_all(&image.bytes)
        .and_then(|()| output.flush())
        .map_err(|error| PlanImageError::InlineImageWrite { error })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use base64::Engine;

    use super::*;

    fn detect(
        env: &[(&str, &str)],
        stdout_is_terminal: bool,
    ) -> Result<InlineImageProtocol, PlanImageError> {
        let env: HashMap<&str, &str> = env.iter().copied().collect();
        detect_inline_image_protocol(
            |name| env.get(name).map(|value| (*value).to_string()),
            stdout_is_terminal,
        )
    }

    #[test]
    fn detects_supported_terminal_environments() {
        let cases = [
            (
                &[("TERM_PROGRAM", "ghostty")][..],
                InlineImageProtocol::Kitty,
            ),
            (&[("TERM", "xterm-kitty")][..], InlineImageProtocol::Kitty),
            (&[("KITTY_WINDOW_ID", "42")][..], InlineImageProtocol::Kitty),
            (
                &[("TERM_PROGRAM", "iTerm.app")][..],
                InlineImageProtocol::Iterm2,
            ),
            (
                &[("LC_TERMINAL", "iTerm2")][..],
                InlineImageProtocol::Iterm2,
            ),
            (
                &[("TERM_PROGRAM", "WezTerm")][..],
                InlineImageProtocol::Iterm2,
            ),
        ];

        for (env, expected) in cases {
            assert_eq!(detect(env, true).unwrap(), expected, "env: {env:?}");
        }
    }

    #[test]
    fn rejects_non_tty_stdout() {
        let error = detect(&[("TERM", "xterm-kitty")], false).unwrap_err();

        assert!(matches!(
            error,
            PlanImageError::InlineImageStdoutNotTerminal
        ));
        let message = error.to_string();
        assert!(message.contains("Kitty"), "{message}");
        assert!(message.contains("Ghostty"), "{message}");
        assert!(message.contains("iTerm2"), "{message}");
        assert!(message.contains("WezTerm"), "{message}");
        assert!(message.contains("--image <FILE>"), "{message}");
    }

    #[test]
    fn rejects_tmux_without_passthrough_support() {
        for env in [
            &[("TERM_PROGRAM", "tmux")][..],
            &[("TMUX", "/tmp/tmux-501/default,123,0")][..],
            &[
                ("TMUX", "/tmp/tmux-501/default,123,0"),
                ("KITTY_WINDOW_ID", "42"),
            ][..],
        ] {
            let error = detect(env, true).unwrap_err();
            assert!(matches!(error, PlanImageError::InlineImageTmuxUnsupported));
            let message = error.to_string();
            assert!(message.contains("tmux"), "{message}");
            assert!(message.contains("passthrough"), "{message}");
        }
    }

    #[test]
    fn rejects_unrecognized_terminal() {
        let error = detect(&[("TERM_PROGRAM", "Apple_Terminal")], true).unwrap_err();

        assert!(matches!(
            error,
            PlanImageError::InlineImageUnsupportedTerminal
        ));
        let message = error.to_string();
        assert!(message.contains("Kitty"), "{message}");
        assert!(message.contains("Ghostty"), "{message}");
        assert!(message.contains("iTerm2"), "{message}");
        assert!(message.contains("WezTerm"), "{message}");
        assert!(message.contains("--image <FILE>"), "{message}");
    }

    fn kitty_commands(bytes: &[u8]) -> Vec<(&str, &str)> {
        let text = std::str::from_utf8(bytes).unwrap();
        assert!(text.ends_with("\n"));
        text.strip_suffix('\n')
            .unwrap()
            .split("\x1b\\")
            .filter(|part| !part.is_empty())
            .map(|part| {
                let command = part.strip_prefix("\x1b_G").unwrap();
                command.split_once(';').unwrap()
            })
            .collect()
    }

    fn inline_request(
        protocol: InlineImageProtocol,
        terminal: Option<TerminalSize>,
    ) -> InlineImageRequest {
        InlineImageRequest { protocol, terminal }
    }

    #[test]
    fn kitty_base64_is_chunked_and_continuation_flags_are_correct() {
        let png = vec![0xabu8; 7_000];
        let rendered =
            encode_inline_image(inline_request(InlineImageProtocol::Kitty, None), &png, 100);
        let commands = kitty_commands(&rendered.bytes);

        assert!(commands.len() >= 3);
        for (_, data) in &commands {
            assert!(data.len() <= 4096, "chunk had {} bytes", data.len());
        }
        assert_eq!(commands[0].0, "a=T,f=100,m=1");
        assert_eq!(commands.last().unwrap().0, "m=0");
        for (index, (control, _)) in commands.iter().enumerate().skip(1) {
            assert!(!control.contains("a=T"), "chunk {index}: {control}");
            assert!(!control.contains("f=100"), "chunk {index}: {control}");
            if index + 1 < commands.len() {
                assert_eq!(*control, "m=1");
            }
        }

        let encoded: String = commands.iter().map(|(_, data)| *data).collect();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            png
        );
    }

    #[test]
    fn kitty_single_chunk_uses_m_zero() {
        let rendered = encode_inline_image(
            inline_request(InlineImageProtocol::Kitty, None),
            b"png",
            100,
        );
        let commands = kitty_commands(&rendered.bytes);

        assert_eq!(commands, vec![("a=T,f=100,m=0", "cG5n")]);
    }

    #[test]
    fn kitty_omits_columns_when_image_fits_terminal_pixels() {
        let rendered = encode_inline_image(
            inline_request(
                InlineImageProtocol::Kitty,
                Some(TerminalSize {
                    columns: 80,
                    pixel_width: Some(800),
                }),
            ),
            b"png",
            400,
        );

        assert_eq!(kitty_commands(&rendered.bytes)[0].0, "a=T,f=100,m=0");
    }

    #[test]
    fn kitty_includes_columns_when_image_is_wider_than_terminal_pixels() {
        let rendered = encode_inline_image(
            inline_request(
                InlineImageProtocol::Kitty,
                Some(TerminalSize {
                    columns: 80,
                    pixel_width: Some(800),
                }),
            ),
            b"png",
            801,
        );

        assert_eq!(kitty_commands(&rendered.bytes)[0].0, "a=T,f=100,c=80,m=0");
    }

    #[test]
    fn kitty_includes_columns_when_terminal_pixel_width_is_unknown() {
        let rendered = encode_inline_image(
            inline_request(
                InlineImageProtocol::Kitty,
                Some(TerminalSize {
                    columns: 80,
                    pixel_width: None,
                }),
            ),
            b"png",
            200,
        );

        assert_eq!(kitty_commands(&rendered.bytes)[0].0, "a=T,f=100,c=80,m=0");
    }

    #[test]
    fn kitty_omits_columns_when_terminal_size_is_unavailable() {
        let rendered = encode_inline_image(
            inline_request(InlineImageProtocol::Kitty, None),
            b"png",
            200,
        );

        assert_eq!(kitty_commands(&rendered.bytes)[0].0, "a=T,f=100,m=0");
    }

    #[test]
    fn iterm2_declares_raw_size_and_base64_round_trips() {
        let png = base64::engine::general_purpose::STANDARD
            .decode(
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
            )
            .unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let rendered =
            encode_inline_image(inline_request(InlineImageProtocol::Iterm2, None), &png, 200);
        let text = std::str::from_utf8(&rendered.bytes).unwrap();
        let prefix = format!(
            "\x1b]1337;File=inline=1;size={};width=100%;preserveAspectRatio=1:",
            png.len()
        );
        let encoded = text
            .strip_prefix(&prefix)
            .unwrap()
            .strip_suffix("\x07\n")
            .unwrap();

        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            png
        );
    }
}
