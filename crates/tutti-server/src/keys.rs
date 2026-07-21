//! Translate the symbolic key names accepted by `pane send --keys` into the
//! raw bytes a terminal would deliver. Whitespace separates a sequence of
//! keys; an unknown name is an error rather than a silent drop.

use anyhow::{Result, bail};

pub fn to_bytes(spec: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for name in spec.split_whitespace() {
        out.extend_from_slice(&one(name)?);
    }
    if out.is_empty() {
        bail!("no keys in {spec:?}");
    }
    Ok(out)
}

fn one(name: &str) -> Result<Vec<u8>> {
    let bytes = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" | "cr" => vec![b'\r'],
        "tab" => vec![b'\t'],
        "esc" | "escape" => vec![0x1b],
        "space" => vec![b' '],
        "backspace" | "bs" => vec![0x7f],
        "up" => vec![0x1b, b'[', b'A'],
        "down" => vec![0x1b, b'[', b'B'],
        "right" => vec![0x1b, b'[', b'C'],
        "left" => vec![0x1b, b'[', b'D'],
        other => match other.strip_prefix("ctrl-") {
            Some(k) if k.len() == 1 && k.as_bytes()[0].is_ascii_lowercase() => {
                vec![k.as_bytes()[0] - b'a' + 1]
            }
            _ => bail!("unknown key name {name:?}"),
        },
    };
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_and_ctrl() {
        assert_eq!(to_bytes("enter").unwrap(), b"\r");
        assert_eq!(to_bytes("ctrl-c").unwrap(), vec![0x03]);
        assert_eq!(to_bytes("ctrl-a").unwrap(), vec![0x01]);
        assert_eq!(to_bytes("up").unwrap(), vec![0x1b, b'[', b'A']);
    }

    #[test]
    fn sequence_concatenates() {
        assert_eq!(to_bytes("esc tab enter").unwrap(), vec![0x1b, b'\t', b'\r']);
    }

    #[test]
    fn unknown_key_fails() {
        assert!(to_bytes("hyper-x").is_err());
        assert!(to_bytes("ctrl-1").is_err());
        assert!(to_bytes("   ").is_err());
    }
}
