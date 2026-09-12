use super::*;

#[test]
fn spatial_vector_and_temporal_constructors_compose() {
    let connection = fresh_storage();
    let result = rows(
        &connection,
        "RETURN point.distance(point({x: 0.0, y: 0.0}), point({x: 3.0, y: 4.0})) AS distance, vector_distance(vector([0.0, 0.0], 2, FLOAT64), vector([3.0, 4.0], 2, FLOAT64), EUCLIDEAN) AS vectorDistance, date('2024-02-29') AS date, datetime('2024-03-31T01:30:00+01:00[Europe/Paris]') AS zoned",
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Float(5.0));
    assert_eq!(result[0][1], Value::Float(5.0));
    assert!(matches!(result[0][2], Value::Date(ref value) if value.as_str() == "2024-02-29"));
    assert!(
        matches!(result[0][3], Value::ZonedDateTime(ref value) if value.zone() == "Europe/Paris")
    );
}

#[test]
fn spatial_point_edges_match_the_frozen_profile() {
    let connection = fresh_storage();
    assert_eq!(
        rows(&connection, "RETURN point({x: null, y: 1}) AS point"),
        vec![vec![Value::Null]]
    );
    assert_eq!(
        rows(
            &connection,
            "RETURN point.withinBBox(point({longitude: 180, latitude: 55.66}), point({longitude: 179, latitude: 55.66}), point({longitude: -179, latitude: 55.70})) AS inside",
        ),
        vec![vec![Value::Boolean(true)]]
    );
    assert_eq!(
        rows(
            &connection,
            "RETURN point({x: 0, y: 0}) < point({x: 1, y: 1}) AS compared",
        ),
        vec![vec![Value::Null]]
    );
    let result = rows(
        &connection,
        "RETURN point.distance(point({longitude: 12.78, latitude: 56.7, height: 100}), point({latitude: 56.71, longitude: 12.79, height: 100})) AS distance",
    );
    let Value::Float(distance) = result[0][0] else {
        panic!("point.distance() must return Float");
    };
    assert!((distance - 1269.9148706779097).abs() < 1.0e-9);
    assert!(
        query_error(
            &connection,
            "RETURN point({x: 1, y: 2, crs: 'cartesian', srid: 7203}) AS point",
        )
        .contains("crs")
    );
    assert!(
        query_error(
            &connection,
            "RETURN point({x: 1, y: 2, longitude: 3, latitude: 4}) AS point",
        )
        .contains("coordinates")
    );
    assert!(
        query_error(
            &connection,
            "RETURN point({longitude: 1, latitude: 2, height: 3, z: 4}) AS point",
        )
        .contains("height")
    );
    assert!(
        query_error(
            &connection,
            "RETURN point({longitude: 3, latitude: 4, crs: 'cartesian'}) AS point",
        )
        .contains("coordinate system")
    );
    assert!(
        query_error(&connection, "RETURN point({x: 1, y: 2}).longitude AS value")
            .contains("longitude")
    );
    assert!(query_error(&connection, "RETURN point({x: 1, y: 2}).z AS value").contains("z"));
    assert!(
        query_error(
            &connection,
            "RETURN point({longitude: 1, latitude: 2}).height AS value",
        )
        .contains("height")
    );
}
