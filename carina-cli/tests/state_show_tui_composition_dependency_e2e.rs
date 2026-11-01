//! Regression coverage for state display rows whose historical `depends_on`
//! target is a module-call composition rather than another persisted row.

use std::io::{Read, Write};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

#[test]
fn state_show_tui_accepts_depends_on_module_call_in_multifile_directory() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/state_show_tui_composition_dependency");
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_carina"));
    for argument in ["state", "show"] {
        command.arg(argument);
    }
    command.arg(fixture.as_os_str());
    command.arg("--tui");
    command.cwd(&fixture);
    command.env("NO_COLOR", "1");
    command.env_remove("CLICOLOR_FORCE");
    command.env_remove("RUST_LOG");
    let mut child = pty
        .slave
        .spawn_command(command)
        .expect("spawn state TUI under pty");
    drop(pty.slave);
    let mut writer = pty.master.take_writer().expect("pty writer");
    writer.write_all(b"q").expect("quit TUI");
    writer.flush().expect("flush TUI input");
    drop(writer);
    let mut reader = pty.master.try_clone_reader().expect("pty reader");
    let mut output = Vec::new();
    reader.read_to_end(&mut output).expect("read TUI output");
    let status = child.wait().expect("wait for state TUI");
    let output = String::from_utf8_lossy(&output);

    assert!(
        status.success(),
        "state show --tui must treat persisted depends_on as display metadata; output:\n{output}",
    );
    assert!(
        output.contains("\x1b[?1049h") && output.contains("consumer"),
        "state show --tui must render the persisted resource in the TUI: {output}",
    );
    assert!(
        !output.contains("not declared in this scope"),
        "display must not resolve module-call dependencies against state rows: {output}",
    );
}
