use super::{QueryError, QueryResult};

#[cfg(test)]
pub(crate) fn has_finite_nonzero_norm(vector: &[f32]) -> bool {
    has_finite_nonzero_norm_controlled(vector, &|| false).unwrap_or(false)
}

pub(crate) fn has_finite_nonzero_norm_controlled(
    vector: &[f32],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<bool> {
    super::super::check_interrupted(is_interrupted)?;
    let norm = sum_f32(vector.iter().map(|value| value * value), is_interrupted)?.sqrt();
    if norm != 0.0 && norm.is_finite() {
        return Ok(true);
    }
    let stable_norm = sum_f64(
        vector.iter().map(|value| {
            let value = f64::from(*value);
            value * value
        }),
        is_interrupted,
    )?
    .sqrt();
    Ok(stable_norm != 0.0 && stable_norm.is_finite())
}

#[cfg(test)]
pub(crate) fn vector_similarity_numbers(
    left: &[f32],
    right: &[f32],
    similarity: &str,
) -> QueryResult<f64> {
    vector_similarity_numbers_controlled(left, right, similarity, &|| false)
}

pub(crate) fn vector_similarity_numbers_controlled(
    left: &[f32],
    right: &[f32],
    similarity: &str,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<f64> {
    super::super::check_interrupted(is_interrupted)?;
    if left.len() != right.len() {
        return Err(QueryError::semantic("vector dimensions must match"));
    }
    match similarity {
        "cosine" => cosine_similarity(left, right, is_interrupted),
        "euclidean" => euclidean_similarity(left, right, is_interrupted),
        other => Err(QueryError::internal(format!(
            "unsupported persisted vector similarity {other:?}"
        ))),
    }
}

fn cosine_similarity(
    left: &[f32],
    right: &[f32],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<f64> {
    let dot = sum_f32(left.iter().zip(right).map(|(a, b)| a * b), is_interrupted)?;
    let left_norm = sum_f32(left.iter().map(|value| value * value), is_interrupted)?.sqrt();
    let right_norm = sum_f32(right.iter().map(|value| value * value), is_interrupted)?.sqrt();
    let denominator = left_norm * right_norm;
    if dot.is_finite() && denominator.is_finite() && denominator != 0.0 {
        return Ok(f64::from(((1.0 + dot / denominator) / 2.0).clamp(0.0, 1.0)));
    }
    stable_cosine_similarity(left, right, is_interrupted)
}

fn stable_cosine_similarity(
    left: &[f32],
    right: &[f32],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<f64> {
    let dot = sum_f64(
        left.iter()
            .zip(right)
            .map(|(left, right)| f64::from(*left) * f64::from(*right)),
        is_interrupted,
    )?;
    let left_norm = sum_f64(
        left.iter().map(|value| f64::from(*value).powi(2)),
        is_interrupted,
    )?
    .sqrt();
    let right_norm = sum_f64(
        right.iter().map(|value| f64::from(*value).powi(2)),
        is_interrupted,
    )?
    .sqrt();
    let denominator = left_norm * right_norm;
    if denominator == 0.0 || !denominator.is_finite() || !dot.is_finite() {
        return Err(QueryError::semantic(
            "cosine similarity requires finite non-zero vectors",
        ));
    }
    Ok(((1.0 + dot / denominator) / 2.0).clamp(0.0, 1.0))
}

fn euclidean_similarity(
    left: &[f32],
    right: &[f32],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<f64> {
    let distance = sum_f32(
        left.iter().zip(right).map(|(a, b)| {
            let delta = a - b;
            delta * delta
        }),
        is_interrupted,
    )?;
    if distance.is_finite() {
        return Ok(f64::from(1.0 / (1.0 + distance)));
    }
    let stable_distance = sum_f64(
        left.iter().zip(right).map(|(left, right)| {
            let delta = f64::from(*left) - f64::from(*right);
            delta * delta
        }),
        is_interrupted,
    )?;
    if !stable_distance.is_finite() {
        return Err(QueryError::semantic(
            "euclidean similarity requires finite vectors",
        ));
    }
    Ok(1.0 / (1.0 + stable_distance))
}

fn sum_f32(
    values: impl Iterator<Item = f32>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<f32> {
    let mut total = 0.0_f32;
    for (position, value) in values.enumerate() {
        if position.is_multiple_of(256) {
            super::super::check_interrupted(is_interrupted)?;
        }
        total += value;
    }
    super::super::check_interrupted(is_interrupted)?;
    Ok(total)
}

fn sum_f64(
    values: impl Iterator<Item = f64>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<f64> {
    let mut total = 0.0_f64;
    for (position, value) in values.enumerate() {
        if position.is_multiple_of(256) {
            super::super::check_interrupted(is_interrupted)?;
        }
        total += value;
    }
    super::super::check_interrupted(is_interrupted)?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryErrorKind;
    use std::cell::Cell;

    #[test]
    fn long_vector_similarity_checks_cancellation_within_coordinates() {
        let vector = vec![1.0; 65_536];
        for similarity in ["cosine", "euclidean"] {
            let calls = Cell::new(0);
            let error = vector_similarity_numbers_controlled(&vector, &vector, similarity, &|| {
                calls.set(calls.get() + 1);
                calls.get() == 4
            })
            .expect_err("cancel within one vector comparison");
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
            assert_eq!(calls.get(), 4);
        }
    }

    #[test]
    fn controlled_similarity_keeps_f32_and_stable_f64_scores() {
        let magnitude = 1.0e20_f32;
        for (left, right, euclidean) in [
            (vec![1.0, 0.0], vec![0.0, 1.0], f64::from(1.0_f32 / 3.0)),
            (
                vec![magnitude, 0.0],
                vec![0.0, magnitude],
                1.0 / (1.0 + 2.0 * f64::from(magnitude).powi(2)),
            ),
        ] {
            for (similarity, expected) in [("cosine", 0.5_f64), ("euclidean", euclidean)] {
                let actual =
                    vector_similarity_numbers_controlled(&left, &right, similarity, &|| false)
                        .expect("controlled similarity");
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
        }
    }

    #[test]
    fn long_norm_validation_checks_cancellation_within_coordinates() {
        let vector = vec![1.0; 65_536];
        let calls = Cell::new(0);
        let error = has_finite_nonzero_norm_controlled(&vector, &|| {
            calls.set(calls.get() + 1);
            calls.get() == 4
        })
        .expect_err("cancel while validating one vector norm");
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(calls.get(), 4);
    }
}
