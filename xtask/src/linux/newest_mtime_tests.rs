use super::newest_mtime_under;
use std::fs;
use std::time::{Duration, SystemTime};

/// A file in a subdirectory counts: `main_tests/` and any future
/// submodule directory must trigger a rebuild like `src/` itself.
#[test]
fn a_nested_file_is_found_and_the_newest_wins() {
    let root = std::env::temp_dir().join(format!("newest-mtime-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("a/b")).unwrap();
    let old = root.join("top.rs");
    let new = root.join("a/b/deep.rs");
    fs::write(&old, "").unwrap();
    fs::write(&new, "").unwrap();
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let t1 = t0 + Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(t0)
        .unwrap();
    fs::File::options()
        .write(true)
        .open(&new)
        .unwrap()
        .set_modified(t1)
        .unwrap();
    assert_eq!(newest_mtime_under(&root), Some(t1));
    assert_eq!(newest_mtime_under(&root.join("missing")), None);
    fs::remove_dir_all(&root).unwrap();
}
