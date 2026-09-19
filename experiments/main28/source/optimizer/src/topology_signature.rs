//! Canonical structural signatures used to balance topology diversity with
//! multi-start sizing exploration.

/// Return a deterministic structural signature for eclass/enode choices.
pub fn topology_signature(entries: &[(String, String)]) -> String {
    let mut normalized: Vec<_> = entries
        .iter()
        .map(|(eclass, enode)| format!("{}={}", eclass, canonical_enode(enode)))
        .collect();
    normalized.sort();
    normalized.join(";")
}

fn canonical_enode(enode_id: &str) -> String {
    let mut expression = enode_id;
    if let Some(rest) = expression.strip_prefix("root=") {
        expression = rest.split_once('|').map_or(rest, |(_, value)| value);
    }
    if let Some(rest) = expression.strip_suffix("|incumbent") {
        expression = rest;
    }
    let mut parser = Parser::new(expression);
    parser.parse_expr()
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
        }
    }

    fn parse_expr(&mut self) -> String {
        self.skip_space();
        let start = self.pos;
        while self.pos < self.input.len()
            && !matches!(self.input[self.pos], b'(' | b')' | b',' | b' ' | b'\t')
        {
            self.pos += 1;
        }
        let raw_name = String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
        let name = normalize_cell_name(&raw_name);
        self.skip_space();
        if self.pos >= self.input.len() || self.input[self.pos] != b'(' {
            return name;
        }
        self.pos += 1;
        let mut args = Vec::new();
        loop {
            self.skip_space();
            if self.pos >= self.input.len() || self.input[self.pos] == b')' {
                if self.pos < self.input.len() {
                    self.pos += 1;
                }
                break;
            }
            args.push(self.parse_expr());
            self.skip_space();
            if self.pos < self.input.len() && self.input[self.pos] == b',' {
                self.pos += 1;
            }
        }
        canonicalize_commutative_args(&name, &mut args);
        format!("{}({})", name, args.join(","))
    }

    fn skip_space(&mut self) {
        while self.pos < self.input.len() && self.input[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }
}

fn normalize_cell_name(name: &str) -> String {
    let Some(lib_marker) = name.find("_ASAP") else {
        return name.to_owned();
    };
    let prefix = &name[..lib_marker];
    let Some(marker) = prefix.rfind('x') else {
        return name.to_owned();
    };
    let tail = &prefix[marker + 1..];
    let digit_start = usize::from(tail.starts_with('p'));
    let digits = &tail[digit_start..];
    let digit_len = digits
        .as_bytes()
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digit_len == 0 {
        return name.to_owned();
    }
    format!(
        "{}{}{}*{}",
        &prefix[..marker],
        &prefix[marker..marker + 1],
        &tail[..digit_start],
        &name[lib_marker..],
    )
}

fn canonicalize_commutative_args(name: &str, args: &mut [String]) {
    let Some(logic_name) = name.split("_ASAP").next() else {
        return;
    };
    let logic_name = logic_name
        .split_once('x')
        .map_or(logic_name, |(prefix, _)| prefix);
    let mut groups = Vec::<Vec<usize>>::new();
    if matches!(
        logic_name,
        "AND2"
            | "AND3"
            | "AND4"
            | "OR2"
            | "OR3"
            | "OR4"
            | "NAND2"
            | "NAND3"
            | "NAND4"
            | "NOR2"
            | "NOR3"
            | "NOR4"
            | "XOR2"
            | "XNOR2"
            | "MAJ"
    ) {
        groups.push((0..args.len()).collect());
    } else if matches!(logic_name, "AO21" | "AOI21" | "OA21" | "OAI21") {
        groups.push((0..args.len().min(2)).collect());
    } else if matches!(logic_name, "AO22" | "AOI22" | "OA22" | "OAI22") {
        groups.push((0..args.len().min(2)).collect());
        if args.len() > 2 {
            groups.push((2..args.len().min(4)).collect());
        }
    }
    for group in groups {
        let mut values: Vec<_> = group.iter().map(|index| args[*index].clone()).collect();
        values.sort();
        for (index, value) in group.into_iter().zip(values) {
            args[index] = value;
        }
    }
}

/// Select rows with a small per-topology quota.
///
/// The first two slots deliberately remain available to the best topology:
/// this preserves the useful multi-start sizing behavior of the original D1
/// flow.  The remaining slots prefer previously unseen topology skeletons,
/// then fall back to score order while never exceeding `per_topology_cap`.
pub fn capped_topology_indices(
    signatures: &[String],
    keep: usize,
    per_topology_cap: usize,
) -> Vec<usize> {
    let keep = keep.min(signatures.len());
    if keep == 0 || per_topology_cap == 0 {
        return Vec::new();
    }
    let mut selected = Vec::with_capacity(keep);
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();

    // Preserve the global winner and, when available, its second sizing
    // start. This is the key difference from strict one-per-topology
    // stratification.
    selected.push(0);
    *counts.entry(&signatures[0]).or_default() += 1;
    if selected.len() < keep && per_topology_cap >= 2 {
        if let Some(index) = signatures
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, signature)| (signature == &signatures[0]).then_some(index))
        {
            selected.push(index);
            *counts.entry(&signatures[index]).or_default() += 1;
        }
    }

    // Spend the remaining slots on new structural topologies first.
    for (index, signature) in signatures.iter().enumerate() {
        if selected.len() == keep {
            break;
        }
        if counts.contains_key(signature.as_str()) {
            continue;
        }
        selected.push(index);
        counts.insert(signature.as_str(), 1);
    }

    // If there are fewer topologies than slots, recover additional sizing
    // starts, subject to the explicit per-topology cap.
    if selected.len() < keep {
        for (index, signature) in signatures.iter().enumerate() {
            if selected.len() == keep {
                break;
            }
            if selected.contains(&index) {
                continue;
            }
            let count = counts.entry(signature.as_str()).or_default();
            if *count < per_topology_cap {
                selected.push(index);
                *count += 1;
            }
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::{capped_topology_indices, topology_signature};

    #[test]
    fn drive_variants_share_signature() {
        let x1 = vec![(
            "occurrence:7".to_owned(),
            "root=7|AND2xp5_ASAP7_6t_L(@1,@2)".to_owned(),
        )];
        let x4 = vec![(
            "occurrence:7".to_owned(),
            "root=7|AND2xp25_ASAP7_6t_L(@1,@2)".to_owned(),
        )];
        assert_eq!(topology_signature(&x1), topology_signature(&x4));
        let implementation_variant = vec![(
            "occurrence:7".to_owned(),
            "root=7|AND2xp25R_ASAP7_6t_L(@1,@2)".to_owned(),
        )];
        assert_eq!(
            topology_signature(&x1),
            topology_signature(&implementation_variant)
        );
    }

    #[test]
    fn commutation_shares_signature_but_roles_do_not() {
        let first = vec![(
            "occurrence:7".to_owned(),
            "root=7|AO22xp5_ASAP7_6t_L(@1,@2,@3,@4)".to_owned(),
        )];
        let swapped = vec![(
            "occurrence:7".to_owned(),
            "root=7|AO22xp25_ASAP7_6t_L(@2,@1,@4,@3)".to_owned(),
        )];
        let role_changed = vec![(
            "occurrence:7".to_owned(),
            "root=7|AO22xp25_ASAP7_6t_L(@1,@3,@2,@4)".to_owned(),
        )];
        assert_eq!(topology_signature(&first), topology_signature(&swapped));
        assert_ne!(
            topology_signature(&first),
            topology_signature(&role_changed)
        );
    }

    #[test]
    fn capped_selection_keeps_two_starts_then_new_skeletons() {
        let signatures = vec!["A".into(), "A".into(), "B".into(), "C".into()];
        assert_eq!(capped_topology_indices(&signatures, 4, 2), vec![0, 1, 2, 3]);
    }

    #[test]
    fn capped_selection_never_exceeds_quota() {
        let signatures = vec!["A".into(), "A".into(), "A".into(), "B".into()];
        let selected = capped_topology_indices(&signatures, 4, 2);
        assert_eq!(selected, vec![0, 1, 3]);
    }
}
