use std::fmt::Display;

use schemars::JsonSchema;
use schemars::r#gen::SchemaGenerator;
use schemars::schema::Schema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, TS, Hash)]
#[ts(type = "string")]
pub struct ConversationId {
    uuid: Uuid,
}

impl ConversationId {
    pub fn new() -> Self {
        Self {
            uuid: Uuid::now_v7(),
        }
    }

    pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self {
            uuid: Uuid::parse_str(s)?,
        })
    }
}

impl Default for ConversationId {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.uuid)
    }
}

impl Serialize for ConversationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(&self.uuid)
    }
}

impl<'de> Deserialize<'de> for ConversationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let uuid = Uuid::parse_str(&value).map_err(serde::de::Error::custom)?;
        Ok(Self { uuid })
    }
}

impl JsonSchema for ConversationId {
    fn schema_name() -> String {
        "ConversationId".to_string()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        <String>::json_schema(generator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_conversation_id_default_is_not_zeroes() {
        let id = ConversationId::default();
        assert_ne!(id.uuid, Uuid::nil());
    }
}
