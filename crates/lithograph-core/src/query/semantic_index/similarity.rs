use super::{QueryError, QueryResult};

pub(crate) fn has_finite_nonzero_norm(vector: &[f32]) -> bool {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm != 0.0 && norm.is_finite() {
        return true;
    }
    let stable_norm = vector
        .iter()
        .map(|value| {
            let value = f64::from(*value);
            value * value
        })
        .sum::<f64>()
        .sqrt();
    stable_norm != 0.0 && stable_norm.is_finite()
}

pub(crate) fn vector_similarity_numbers(
    left: &[f32],
    right: &[f32],
    similarity: &str,
) -> QueryResult<f64> {
    if left.len() != right.len() {
        return Err(QueryError::semantic("vector dimensions must match"));
    }
    match similarity {
        "cosine" => cosine_similarity(left, right),
        "euclidean" => euclidean_similarity(left, right),
        other => Err(QueryError::internal(format!(
            "unsupported persisted vector similarity {other:?}"
        ))),
    }
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> QueryResult<f64> {
    let dot = left.iter().zip(right).map(|(a, b)| a * b).sum::<f32>();
    let left_norm = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let right_norm = right.iter().map(|value| value * value).sum::<f32>().sqrt();
    let denominator = left_norm * right_norm;
    if dot.is_finite() && denominator.is_finite() && denominator != 0.0 {
        return Ok(f64::from(((1.0 + dot / denominator) / 2.0).clamp(0.0, 1.0)));
    }
    stable_cosine_similarity(left, right)
}

fn stable_cosine_similarity(left: &[f32], right: &[f32]) -> QueryResult<f64> {
    let dot = left
        .iter()
        .zip(right)
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum::<f64>();
    let left_norm = left
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    let right_norm = right
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    let denominator = left_norm * right_norm;
    if denominator == 0.0 || !denominator.is_finite() || !dot.is_finite() {
        return Err(QueryError::semantic(
            "cosine similarity requires finite non-zero vectors",
        ));
    }
    Ok(((1.0 + dot / denominator) / 2.0).clamp(0.0, 1.0))
}

fn euclidean_similarity(left: &[f32], right: &[f32]) -> QueryResult<f64> {
    let distance = left
        .iter()
        .zip(right)
        .map(|(a, b)| {
            let delta = a - b;
            delta * delta
        })
        .sum::<f32>();
    if distance.is_finite() {
        return Ok(f64::from(1.0 / (1.0 + distance)));
    }
    let stable_distance = left
        .iter()
        .zip(right)
        .map(|(left, right)| {
            let delta = f64::from(*left) - f64::from(*right);
            delta * delta
        })
        .sum::<f64>();
    if !stable_distance.is_finite() {
        return Err(QueryError::semantic(
            "euclidean similarity requires finite vectors",
        ));
    }
    Ok(1.0 / (1.0 + stable_distance))
}
