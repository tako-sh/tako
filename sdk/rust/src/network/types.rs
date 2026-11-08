//! JSON field presence helpers used by `tako gen`.
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Distinguishes an absent property from a present one. For a nullable property,
/// `Optional<Option<T>>` distinguishes missing, null, and a value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Optional<T> {
    #[default]
    Missing,
    Value(T),
}
impl<T> Optional<T> {
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}
impl<T: Serialize> Serialize for Optional<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing => Err(serde::ser::Error::custom("missing field must be omitted")),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Optional<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Value)
    }
}
/// Deserialize a required field, including required nullable fields.
pub fn required<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    T::deserialize(deserializer)
}

/// An uninhabited JSON type (`false` in JSON Schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Never {}
