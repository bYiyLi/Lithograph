use super::types::Key;
use super::*;

impl Analysis<'_, '_, '_, '_> {
    /// The complete WHERE has already passed totality. Hash compatibility is
    /// independent of optional Index definitions and their TEMP capabilities.
    pub(super) fn hash_equality(
        &mut self,
        predicate: usize,
        left: Operand<'_>,
        right: Operand<'_>,
    ) -> QueryResult<bool> {
        let family = match left.family {
            Family::Boolean => HashFamily::Boolean,
            Family::Number => HashFamily::Number,
            Family::String => HashFamily::String,
            Family::Node => HashFamily::NodeIdentity,
            Family::Relationship => HashFamily::RelationshipIdentity,
            _ => return Ok(true),
        };
        let (Some(left_key), Some(right_key)) = (left.key, right.key) else {
            return Ok(true);
        };
        if left.family != right.family
            || left.providers.0 == 0
            || right.providers.0 == 0
            || (left.providers.0 | right.providers.0).count_ones() < 2
        {
            return Ok(true);
        }
        let names = key_bytes(left_key).saturating_add(key_bytes(right_key));
        if !self.work(
            1,
            (2 * std::mem::size_of::<HashEquality>()).saturating_add(names.saturating_mul(2)),
        )? {
            return Ok(false);
        }
        self.hashes.push(HashEquality {
            predicate,
            left: input_key(left_key, left.providers),
            right: input_key(right_key, right.providers),
            family,
        });
        Ok(true)
    }
}

fn key_bytes(key: Key<'_>) -> usize {
    match key {
        Key::Entity(variable) => variable.len(),
        Key::Property { variable, property } => variable.len().saturating_add(property.len()),
    }
}

fn input_key(key: Key<'_>, providers: PartSet) -> HashInputKey {
    let (variable, property) = match key {
        Key::Entity(variable) => (variable, None),
        Key::Property { variable, property } => (variable, Some(property.to_owned())),
    };
    HashInputKey {
        variable: variable.to_owned(),
        property,
        providers,
    }
}
