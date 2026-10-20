use std::path::Path;

/// Writes an executable `#!$BIT_TEST_SHELL` script with `body`. A child shell
/// writes it: if this process held the write descriptor, a check spawned
/// concurrently by another test thread could inherit it across fork, and
/// executing the script would then fail with ETXTBSY ("Text file busy").
pub fn write_script(path: &Path, body: &str) {
    let shell = std::env::var("BIT_TEST_SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let status = std::process::Command::new(&shell)
        .args([
            "-c",
            "printf '%s' \"$1\" > \"$2\" && chmod 755 \"$2\"",
            "sh",
        ])
        .arg(format!("#!{shell}\n{body}\n"))
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "writing {}", path.display());
}
