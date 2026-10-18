use super::*;
use crate::query::resources::{QueryResources, value_bytes};

#[test]
fn windows_drive_file_url_path_strips_only_standard_drive_prefix() {
    assert_eq!(
        windows_drive_file_url_path("/D:/work/phase08.csv"),
        Some("D:/work/phase08.csv")
    );
    assert_eq!(windows_drive_file_url_path("/tmp/phase08.csv"), None);
    assert_eq!(windows_drive_file_url_path("//server/share.csv"), None);
}

#[test]
fn headers_and_live_record_memory_survive_until_their_owners_are_dropped() {
    let resources = QueryResources::new(Some(256 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut file = tempfile::NamedTempFile::new().expect("CSV fixture");
    file.write_all("名字,role\n码哈,partner\n主人,author\n".as_bytes())
        .expect("CSV fixture data");
    let source = format!("file://{}", file.path().display());
    let mut stream = CsvStream::open(&source, ',', true, &control).expect("CSV stream");
    let base = resources.used();
    let (value, line, memory) = stream
        .next_value(&control)
        .expect("record")
        .expect("CSV data");
    assert_eq!(line, 2);
    assert_eq!(
        memory.bytes(),
        value_bytes(&value).expect("CSV value bytes")
    );
    let Value::Map(values) = &value else {
        panic!("headers must produce a map");
    };
    assert_eq!(values.get("名字"), Some(&Value::String("码哈".to_owned())));
    let live = resources.used();
    drop(value);
    drop(memory);
    assert!(resources.used() > base, "headers stay owned by the stream");
    assert!(
        resources.used() < live,
        "returned record releases its allocation"
    );
    drop(stream);
    assert_eq!(resources.used(), 0);
}

#[test]
fn csv_binding_moves_the_value_claim_and_covers_input_clone_and_context() {
    let resources = QueryResources::new(Some(32 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let value = Value::String("人物".repeat(256));
    let value_memory = control
        .reserve(value_bytes(&value).expect("value bytes"))
        .expect("value claim");
    let (row, memory) = csv_binding_row(
        &BindingRow::default(),
        "record",
        value,
        "/synthetic/story.csv",
        3,
        value_memory,
        &control,
    )
    .expect("CSV binding");
    assert_eq!(
        memory.bytes(),
        expression::binding_row_bytes(&row).expect("binding bytes")
    );
    assert_eq!(
        row.load_csv_context
            .as_ref()
            .and_then(|context| context.line),
        Some(3)
    );
    drop(row);
    drop(memory);
    assert_eq!(resources.used(), 0);
}
