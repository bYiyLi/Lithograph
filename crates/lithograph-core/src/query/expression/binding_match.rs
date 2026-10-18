//! Compatibility checks shared by fixed and quantified relationship binding.

use super::{BindingRow, BindingValue, QueryError, QueryResult, RelationshipRecord};

impl BindingRow {
    /// The caller accounts for an absent binding before asking to insert it.
    pub(crate) fn bind_relationship(
        &mut self,
        variable: Option<&str>,
        relationship: RelationshipRecord,
    ) -> QueryResult<bool> {
        let Some(variable) = variable else {
            return Ok(true);
        };
        match self.values.get(variable) {
            Some(BindingValue::Relationship(existing)) => Ok(existing.id == relationship.id),
            Some(BindingValue::Null) => Ok(false),
            Some(_) => Err(QueryError::semantic(format!(
                "variable {variable} is not a Relationship binding"
            ))),
            None => {
                self.insert(
                    variable.to_owned(),
                    BindingValue::Relationship(relationship),
                );
                Ok(true)
            }
        }
    }
}
