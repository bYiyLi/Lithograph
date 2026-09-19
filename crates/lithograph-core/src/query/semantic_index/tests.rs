use super::{has_finite_nonzero_norm, vector_similarity_numbers};

#[test]
fn cosine_extreme_finite_vectors_keep_finite_scores() {
    let magnitude = 1.0e20_f32;
    let identical =
        vector_similarity_numbers(&[magnitude, magnitude], &[magnitude, magnitude], "cosine")
            .expect("large identical cosine");
    assert!(identical.is_finite());
    assert!((identical - 1.0).abs() < f64::EPSILON);

    let orthogonal = vector_similarity_numbers(&[magnitude, 0.0], &[0.0, magnitude], "cosine")
        .expect("large orthogonal cosine");
    assert!(orthogonal.is_finite());
    assert!((orthogonal - 0.5).abs() < f64::EPSILON);
    assert!(has_finite_nonzero_norm(&[magnitude, magnitude]));
}

#[test]
fn cosine_zero_vector_remains_invalid() {
    assert!(!has_finite_nonzero_norm(&[0.0, 0.0]));
    assert!(vector_similarity_numbers(&[0.0, 0.0], &[1.0, 0.0], "cosine").is_err());
}

#[test]
fn euclidean_extreme_finite_vectors_keep_finite_nonzero_scores() {
    let magnitude = 1.0e20_f32;
    let score = vector_similarity_numbers(&[magnitude, 0.0], &[-magnitude, 0.0], "euclidean")
        .expect("large euclidean");
    assert!(score.is_finite());
    assert!(score > 0.0);
    assert!(score < 1.0);
}
