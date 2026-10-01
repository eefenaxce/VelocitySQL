//! Output columns: the "row description" of a result set.

use vsql_types::DataType;

/// One column of an operator's output.
///
/// A field carries more than a name and a type: `qualifier` preserves the table
/// name (or alias) a column came from, which is what makes
/// `SELECT u.id FROM users u` and `SELECT o.id FROM orders o` distinguishable
/// while both are in scope, and what `SELECT u.*` expands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    /// Table name or alias the column originates from.
    pub qualifier: Option<String>,
    /// Column name.
    pub name: String,
    /// Column type.
    pub data_type: DataType,
    /// `true` when the column must not be expanded by `SELECT *`.
    ///
    /// Set for the duplicate half of a `USING`/`NATURAL` join key: the column is
    /// still addressable as `right.id`, but `SELECT *` lists it once.
    pub hidden: bool,
}

impl Field {
    /// Builds a field with an explicit qualifier.
    pub fn new(qualifier: Option<String>, name: impl Into<String>, data_type: DataType) -> Field {
        Field {
            qualifier,
            name: name.into(),
            data_type,
            hidden: false,
        }
    }

    /// Returns a copy marked as hidden from `SELECT *`.
    pub fn hidden(mut self) -> Field {
        self.hidden = true;
        self
    }

    /// Builds an unqualified field.
    pub fn unqualified(name: impl Into<String>, data_type: DataType) -> Field {
        Field::new(None, name, data_type)
    }

    /// Builds an unnamed expression field, the way PostgreSQL labels
    /// `SELECT 1 + 1` as `?column?`.
    pub fn expression(data_type: DataType) -> Field {
        Field::unqualified("?column?", data_type)
    }
}

/// A list of fields, i.e. the schema of an operator.
pub type Schema = Vec<Field>;

/// Finds a column in `schema` by name, honouring an optional qualifier.
///
/// Returns `Err` with the PostgreSQL wording when the reference is missing or
/// ambiguous, so callers can propagate it unchanged.
pub fn resolve_column<'a>(
    schema: &'a [Field],
    qualifier: Option<&str>,
    name: &str,
) -> Result<&'a Field, crate::error::ExecError> {
    let matches: Vec<&Field> = schema
        .iter()
        .filter(|field| {
            field.name.eq_ignore_ascii_case(name)
                && match qualifier {
                    None => true,
                    Some(qualifier) => field
                        .qualifier
                        .as_deref()
                        .map(|q| q.eq_ignore_ascii_case(qualifier))
                        .unwrap_or(false),
                }
        })
        .collect();
    match matches.as_slice() {
        [] => Err(crate::error::ExecError::UndefinedColumn(name.to_string())),
        [field] => Ok(field),
        _ => Err(crate::error::ExecError::AmbiguousColumn(name.to_string())),
    }
}
