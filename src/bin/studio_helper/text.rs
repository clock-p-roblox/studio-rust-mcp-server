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
        format!("{}...", &trimmed[..LIMIT])
    }
}
