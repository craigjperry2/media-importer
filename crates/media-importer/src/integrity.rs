use std::path::Path;

use color_eyre::Result;
use color_eyre::eyre::eyre;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct IntegrityFinding {
    pub category: &'static str,
    sort_identity: Vec<u8>,
    pub identity: String,
    pub details: String,
}

impl IntegrityFinding {
    pub(crate) fn new(
        category: &'static str,
        identity: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        let identity = identity.into();
        Self {
            category,
            sort_identity: identity.as_bytes().to_vec(),
            identity,
            details: details.into(),
        }
    }

    pub(crate) fn for_path(
        category: &'static str,
        path: &Path,
        details: impl Into<String>,
    ) -> Self {
        Self {
            category,
            sort_identity: path_bytes(path),
            identity: escape_path(path),
            details: details.into(),
        }
    }
}

pub(crate) fn push_finding(
    findings: &mut Vec<IntegrityFinding>,
    finding: IntegrityFinding,
) -> Result<()> {
    findings
        .try_reserve(1)
        .map_err(|error| eyre!("reserve integrity finding: {error}"))?;
    findings.push(finding);
    Ok(())
}

pub(crate) fn sort_and_deduplicate(findings: &mut Vec<IntegrityFinding>) {
    findings.sort();
    findings.dedup_by(|left, right| {
        left.category == right.category && left.sort_identity == right.sort_identity
    });
}

pub(crate) fn escape_path(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        escape_bytes(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    escape_bytes(path.to_string_lossy().as_bytes())
}

fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    path.to_string_lossy().into_owned().into_bytes()
}

fn escape_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut output = String::new();
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                push_valid_text(&mut output, valid);
                break;
            }
            Err(error) => {
                let valid = &remaining[..error.valid_up_to()];
                push_valid_text(
                    &mut output,
                    std::str::from_utf8(valid).expect("UTF-8 valid prefix"),
                );
                let invalid_len = error
                    .error_len()
                    .unwrap_or(remaining.len() - error.valid_up_to())
                    .max(1);
                for byte in &remaining[error.valid_up_to()..error.valid_up_to() + invalid_len] {
                    write!(output, "\\x{byte:02x}").expect("writing to String cannot fail");
                }
                remaining = &remaining[error.valid_up_to() + invalid_len..];
            }
        }
    }
    output
}

fn push_valid_text(output: &mut String, text: &str) {
    use std::fmt::Write;

    for character in text.chars() {
        if character.is_control() || character.is_whitespace() {
            let mut encoded = [0; 4];
            for byte in character.encode_utf8(&mut encoded).as_bytes() {
                write!(output, "\\x{byte:02x}").expect("writing to String cannot fail");
            }
        } else {
            output.push(character);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn raw_path_identity_escapes_non_utf8_and_control_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"bad\xff name\n"));
        assert_eq!(escape_path(path), "bad\\xff\\x20name\\x0a");
    }
}
