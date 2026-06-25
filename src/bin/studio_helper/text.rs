use color_eyre::eyre::{eyre, Result};

pub(super) fn trim(value: &str) -> String {
    value.trim().to_owned()
}

pub(super) fn sanitize_place_id(value: &str) -> Result<String> {
    let trimmed = trim(value);
    if trimmed.is_empty() || !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(eyre!("placeId must be digits only"));
    }
    Ok(trimmed)
}

pub(super) fn sanitize_identifier(label: &str, value: &str) -> Result<String> {
    let trimmed = trim(value);
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(eyre!("{label} must use [A-Za-z0-9_-] only"));
    }
    Ok(trimmed)
}

pub(super) fn summarize_error(value: &str) -> String {
    const LIMIT: usize = 180;
    let trimmed = value.trim();
    if trimmed.len() <= LIMIT {
        trimmed.to_owned()
    } else {
        let end = trimmed
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= LIMIT)
            .last()
            .unwrap_or(0);
        format!("{}...", &trimmed[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::summarize_error;

    #[test]
    fn summarize_error_keeps_short_ascii_error() {
        assert_eq!(
            summarize_error("  failed to connect  "),
            "failed to connect"
        );
    }

    #[test]
    fn summarize_error_truncates_long_ascii_error() {
        let input = "x".repeat(181);
        assert_eq!(summarize_error(&input), format!("{}...", "x".repeat(180)));
    }

    #[test]
    fn summarize_error_truncates_on_utf8_char_boundary() {
        let input = format!("{}中", "x".repeat(179));
        assert_eq!(input.len(), 182);
        assert_eq!(summarize_error(&input), format!("{}...", "x".repeat(179)));
    }
}
