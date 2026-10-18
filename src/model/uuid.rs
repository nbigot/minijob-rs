//! UUID parsing with the rules of `github.com/google/uuid.Parse`.

use uuid::Uuid;

fn xtob(a: u8, b: u8) -> Option<u8> {
    let h = (a as char).to_digit(16)?;
    let l = (b as char).to_digit(16)?;
    Some(((h << 4) | l) as u8)
}

/// Parses `s` like Go's `uuid.Parse`:
///
/// - `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (36 bytes),
/// - `urn:uuid:` + 36 bytes (prefix compared case-insensitively),
/// - 38 bytes: the 36-byte form between any two characters (Go does not check for braces),
/// - 32 hex digits.
///
/// Hex digits are accepted in any case.
pub fn parse_go(s: &str) -> Result<Uuid, String> {
    let b = s.as_bytes();
    let body: &[u8] = match b.len() {
        36 => b,
        45 => {
            if !b[..9].eq_ignore_ascii_case(b"urn:uuid:") {
                return Err(format!(
                    "invalid urn prefix: {:?}",
                    String::from_utf8_lossy(&b[..9])
                ));
            }
            &b[9..]
        }
        38 => &b[1..],
        32 => {
            let mut out = [0u8; 16];
            for (i, o) in out.iter_mut().enumerate() {
                *o = xtob(b[i * 2], b[i * 2 + 1]).ok_or("invalid UUID format")?;
            }
            return Ok(Uuid::from_bytes(out));
        }
        n => return Err(format!("invalid UUID length: {n}")),
    };
    if body[8] != b'-' || body[13] != b'-' || body[18] != b'-' || body[23] != b'-' {
        return Err("invalid UUID format".to_string());
    }
    let mut out = [0u8; 16];
    for (i, x) in [0, 2, 4, 6, 9, 11, 14, 16, 19, 21, 24, 26, 28, 30, 32, 34]
        .into_iter()
        .enumerate()
    {
        out[i] = xtob(body[x], body[x + 1]).ok_or("invalid UUID format")?;
    }
    Ok(Uuid::from_bytes(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn accepted_forms() {
        let id = Uuid::parse_str(ID).unwrap();
        assert_eq!(parse_go(ID).unwrap(), id);
        assert_eq!(parse_go(&ID.to_uppercase()).unwrap(), id);
        assert_eq!(parse_go(&format!("{{{ID}}}")).unwrap(), id);
        assert_eq!(parse_go(&format!("x{ID}y")).unwrap(), id);
        assert_eq!(parse_go(&format!("URN:UUID:{ID}")).unwrap(), id);
        assert_eq!(parse_go(&ID.replace('-', "")).unwrap(), id);
    }

    #[test]
    fn rejected_forms() {
        assert!(parse_go("invalid-uuid").is_err());
        assert!(parse_go("").is_err());
        assert!(parse_go(&ID.replace('-', "_")).is_err());
        assert!(parse_go(&format!("urn:uuix:{ID}")).is_err());
        assert!(parse_go("550e8400-e29b-41d4-a716-44665544000g").is_err());
        // Multi-byte characters count as several bytes, as in Go.
        assert!(parse_go("550e8400-e29b-41d4-a716-4466554400é").is_err());
    }
}
