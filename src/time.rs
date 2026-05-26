use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

pub(crate) fn deserialize_optional_label<'de, D>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Option<String>>::deserialize(deserializer)
}

pub(crate) fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
