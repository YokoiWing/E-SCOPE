use indexmap::IndexMap;
use libertyparse::{Liberty, PinDirection};
use std::fs;
use std::path::Path;

pub type Library = IndexMap<String, IndexMap<String, PinDirection>>;

pub fn read_liberty<P: AsRef<Path>>(path: P) -> Result<Liberty, String> {
    let content =
        fs::read_to_string(path.as_ref()).map_err(|e| format!("Error reading file: {}", e))?;
    Liberty::parse_str(&content)
}

pub fn get_direction_of_pins(liberty: &Liberty) -> Result<Library, String> {
    Ok(liberty
        .libs
        .first()
        .ok_or("No lib found!".to_string())?
        .1
        .cells
        .iter()
        .map(|(n, c)| {
            (
                n.to_string(),
                c.pins
                    .iter()
                    .map(|(n, p)| (n.to_string(), p.direction.clone()))
                    .collect(),
            )
        })
        .collect())
}

fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'\\')
}

fn find_keyword(bytes: &[u8], mut cursor: usize, end: usize, keyword: &[u8]) -> Option<usize> {
    while cursor + keyword.len() <= end {
        if &bytes[cursor..cursor + keyword.len()] == keyword
            && (cursor == 0 || !identifier_byte(bytes[cursor - 1]))
            && (cursor + keyword.len() == bytes.len()
                || !identifier_byte(bytes[cursor + keyword.len()]))
        {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

fn skip_space(bytes: &[u8], mut cursor: usize, end: usize) -> usize {
    while cursor < end && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    cursor
}

fn group_name(bytes: &[u8], cursor: usize, end: usize) -> Result<(String, usize), String> {
    let mut cursor = skip_space(bytes, cursor, end);
    if bytes.get(cursor) != Some(&b'(') {
        return Err("Liberty group missing '('".into());
    }
    cursor = skip_space(bytes, cursor + 1, end);
    let quoted = bytes.get(cursor) == Some(&b'"');
    if quoted {
        cursor += 1;
    }
    let start = cursor;
    while cursor < end {
        if (quoted && bytes[cursor] == b'"') || (!quoted && bytes[cursor] == b')') {
            break;
        }
        cursor += 1;
    }
    if cursor >= end {
        return Err("unterminated Liberty group name".into());
    }
    let name = std::str::from_utf8(&bytes[start..cursor])
        .map_err(|error| error.to_string())?
        .trim()
        .to_owned();
    if quoted {
        cursor = skip_space(bytes, cursor + 1, end);
        if bytes.get(cursor) != Some(&b')') {
            return Err("quoted Liberty group name missing ')'".into());
        }
    }
    Ok((name, cursor + 1))
}

/// Find the matching close brace while ignoring quoted table data and
/// comments.  FULL-186 contains tens of megabytes of LUT strings; the normal
/// Liberty AST parser constructs all of those tables even when a caller only
/// needs pin directions.
fn matching_brace(bytes: &[u8], open: usize, end: usize) -> Result<usize, String> {
    let mut depth = 0usize;
    let mut cursor = open;
    let mut quoted = false;
    while cursor < end {
        let byte = bytes[cursor];
        if quoted {
            if byte == b'\\' {
                cursor = (cursor + 2).min(end);
                continue;
            }
            if byte == b'"' {
                quoted = false;
            }
            cursor += 1;
            continue;
        }
        if byte == b'"' {
            quoted = true;
            cursor += 1;
            continue;
        }
        if byte == b'/' && bytes.get(cursor + 1) == Some(&b'*') {
            cursor += 2;
            while cursor + 1 < end && !(bytes[cursor] == b'*' && bytes[cursor + 1] == b'/') {
                cursor += 1;
            }
            cursor = (cursor + 2).min(end);
            continue;
        }
        if byte == b'/' && bytes.get(cursor + 1) == Some(&b'/') {
            cursor += 2;
            while cursor < end && bytes[cursor] != b'\n' {
                cursor += 1;
            }
            continue;
        }
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| "unbalanced Liberty close brace".to_owned())?;
                if depth == 0 {
                    return Ok(cursor);
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    Err("unterminated Liberty group".into())
}

/// Parse only `cell/pin/direction` groups in one linear scan.  This is
/// semantically equivalent to `read_liberty` + `get_direction_of_pins` for the
/// single-output combinational libraries accepted by this project, but avoids
/// materializing unrelated NLDM tables a second time in the native pipeline.
pub fn read_pin_directions_fast<P: AsRef<Path>>(path: P) -> Result<Library, String> {
    let content =
        fs::read(path.as_ref()).map_err(|error| format!("Error reading file: {error}"))?;
    let bytes = content.as_slice();
    let mut library = Library::new();
    let mut cursor = 0usize;
    while let Some(cell_at) = find_keyword(bytes, cursor, bytes.len(), b"cell") {
        let (cell_name, after_name) = group_name(bytes, cell_at + 4, bytes.len())?;
        let open = bytes[after_name..]
            .iter()
            .position(|byte| *byte == b'{')
            .map(|offset| after_name + offset)
            .ok_or_else(|| format!("cell {cell_name} missing '{{'"))?;
        let close = matching_brace(bytes, open, bytes.len())?;
        let mut pins = IndexMap::new();
        let mut pin_cursor = open + 1;
        while let Some(pin_at) = find_keyword(bytes, pin_cursor, close, b"pin") {
            let (pin_name, after_pin) = group_name(bytes, pin_at + 3, close)?;
            let pin_open = bytes[after_pin..close]
                .iter()
                .position(|byte| *byte == b'{')
                .map(|offset| after_pin + offset)
                .ok_or_else(|| format!("pin {cell_name}.{pin_name} missing '{{'"))?;
            let pin_close = matching_brace(bytes, pin_open, close)?;
            let direction_at = find_keyword(bytes, pin_open + 1, pin_close, b"direction")
                .ok_or_else(|| format!("pin {cell_name}.{pin_name} missing direction"))?;
            let colon = bytes[direction_at + 9..pin_close]
                .iter()
                .position(|byte| *byte == b':')
                .map(|offset| direction_at + 9 + offset)
                .ok_or_else(|| format!("pin {cell_name}.{pin_name} missing direction ':'"))?;
            let value_start = skip_space(bytes, colon + 1, pin_close);
            let value_end = bytes[value_start..pin_close]
                .iter()
                .position(|byte| *byte == b';' || byte.is_ascii_whitespace())
                .map(|offset| value_start + offset)
                .unwrap_or(pin_close);
            let direction = match &bytes[value_start..value_end] {
                b"input" => PinDirection::I,
                b"output" => PinDirection::O,
                value => {
                    return Err(format!(
                        "unsupported direction {} for {cell_name}.{pin_name}",
                        String::from_utf8_lossy(value)
                    ));
                }
            };
            pins.insert(pin_name, direction);
            pin_cursor = pin_close + 1;
        }
        library.insert(cell_name, pins);
        cursor = close + 1;
    }
    if library.is_empty() {
        return Err("no Liberty cell groups found".into());
    }
    Ok(library)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_liberty() {
        let liberty = read_liberty("test/asap7sc6t_SELECT_LVT_TT_nldm.lib").unwrap();
        assert_eq!(liberty.libs.len(), 1);
        let lib = liberty.libs.first().unwrap();
        println!("{}:", lib.0);
        for cell in lib.1.cells.iter() {
            println!("  {}:", cell.0);
            for pin in cell.1.pins.iter() {
                println!("    {}: {:?}", pin.0, pin.1.direction);
            }
        }
        let pins_direction = get_direction_of_pins(&liberty).unwrap();
        println!("{:?}", pins_direction);
    }

    #[test]
    fn fast_pin_direction_scan_matches_full_parser() {
        for path in [
            "test/asap7sc6t_SELECT_LVT_TT_nldm.lib",
            "test/asap7sc6t_full_comb/asap7sc6t_FULL_COMB_LVT_TT_nldm_211010.lib",
        ] {
            let expected = get_direction_of_pins(&read_liberty(path).unwrap()).unwrap();
            assert_eq!(read_pin_directions_fast(path).unwrap(), expected, "{path}");
        }
    }
}
