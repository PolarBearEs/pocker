pub(crate) fn format_units(amount: u64, base: f64, units: &[&str]) -> String {
    let Some(first_unit) = units.first() else {
        return amount.to_string();
    };

    let mut value = amount as f64;
    let mut unit = 0usize;
    while value >= base && unit + 1 < units.len() {
        value /= base;
        unit += 1;
    }

    if unit == 0 {
        format!("{amount} {first_unit}")
    } else if value >= 10.0 {
        format!("{value:.0} {}", units[unit])
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::format_units;

    #[test]
    fn format_units_uses_requested_base_and_units() {
        assert_eq!(format_units(999, 1000.0, &["B", "kB"]), "999 B");
        assert_eq!(format_units(1500, 1000.0, &["B", "kB"]), "1.5 kB");
        assert_eq!(format_units(1536, 1024.0, &["B", "KiB"]), "1.5 KiB");
    }

    #[test]
    fn format_units_handles_empty_unit_list() {
        assert_eq!(format_units(42, 1000.0, &[]), "42");
    }
}
