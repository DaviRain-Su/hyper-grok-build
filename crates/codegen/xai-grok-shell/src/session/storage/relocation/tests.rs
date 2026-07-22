use std::fs;
use std::os::unix::fs::PermissionsExt;

use nix::sys::stat::Mode;
use nix::unistd::mkfifo;

use super::*;
use crate::session::storage::relocation::journal::AtomicWriteFault;

#[test]
fn journal_is_validated_commit_aware_and_externally_leased() {
    let temp = tempfile::tempdir().unwrap();
    let storage = RelocationStorage::new(temp.path().into());
    let lease = storage.acquire("session").unwrap();
    assert!(matches!(
        storage.acquire("session"),
        Err(RelocationError::LeaseBusy(_))
    ));

    let journal =
        RelocationJournal::test_new("session", "/source", "/target", RelocationPhase::Prepared);
    storage.write_journal(&journal).unwrap();
    assert_eq!(storage.read_journal("session").unwrap(), journal);

    let mut invalid = journal.clone();
    invalid.cwd_generation = 0;
    assert!(matches!(
        storage.write_journal(&invalid),
        Err(WriteFailure::NotCommitted(_))
    ));
    drop(lease);
}

#[test]
fn atomic_journal_write_distinguishes_rename_commit() {
    for (fault, is_committed) in [
        (AtomicWriteFault::BeforeRename, false),
        (AtomicWriteFault::AfterRename, true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let journal =
            RelocationJournal::test_new("session", "/source", "/target", RelocationPhase::Ready);
        let result = super::journal::write(temp.path(), &journal, Some(fault));
        assert_eq!(
            matches!(result, Err(WriteFailure::Committed(_))),
            is_committed
        );
        assert_eq!(
            super::journal::journal_path(temp.path(), "session").exists(),
            is_committed
        );
    }
}

#[test]
fn recursive_copy_preserves_modes_and_symlinks_and_rejects_special_files() {
    let temp = tempfile::tempdir().unwrap();
    let storage = RelocationStorage::new(temp.path().into());
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    fs::create_dir_all(source.join("nested")).unwrap();
    let executable = source.join("nested/tool");
    fs::write(&executable, b"opaque\0bytes").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o751)).unwrap();
    std::os::unix::fs::symlink("nested/tool", source.join("relative-link")).unwrap();
    std::os::unix::fs::symlink("missing", source.join("dangling-link")).unwrap();

    storage.copy_directory(&source, &target).unwrap();
    assert_eq!(
        fs::metadata(target.join("nested/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o751
    );
    assert_eq!(
        fs::read_link(target.join("relative-link")).unwrap(),
        std::path::Path::new("nested/tool")
    );
    assert_eq!(
        fs::read_link(target.join("dangling-link")).unwrap(),
        std::path::Path::new("missing")
    );

    let special_source = temp.path().join("special-source");
    fs::create_dir(&special_source).unwrap();
    mkfifo(&special_source.join("pipe"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    assert!(
        storage
            .copy_directory(&special_source, &temp.path().join("special-target"))
            .is_err()
    );
}

#[test]
fn copy_rejects_existing_target_without_changing_stale_contents() {
    let temp = tempfile::tempdir().unwrap();
    let storage = RelocationStorage::new(temp.path().into());
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("new"), "new").unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(target.join("stale"), "stale").unwrap();

    assert!(matches!(
        storage.copy_directory(&source, &target),
        Err(RelocationError::Collision(_))
    ));
    assert_eq!(fs::read_to_string(target.join("stale")).unwrap(), "stale");
    assert!(!target.join("new").exists());
}

#[test]
fn copy_rejects_same_or_nested_target_before_creation() {
    let temp = tempfile::tempdir().unwrap();
    let storage = RelocationStorage::new(temp.path().into());
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();

    for target in [source.clone(), source.join("nested/../target")] {
        assert!(matches!(
            storage.copy_directory(&source, &target),
            Err(RelocationError::Inconsistent(_))
        ));
        assert!(!source.join("target").exists());
    }
}

#[test]
fn copy_rejects_source_and_target_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let storage = RelocationStorage::new(temp.path().into());
    let source = temp.path().join("source");
    let source_alias = temp.path().join("source-alias");
    let target_parent_alias = temp.path().join("target-parent-alias");
    fs::create_dir(&source).unwrap();
    std::os::unix::fs::symlink(&source, &source_alias).unwrap();
    std::os::unix::fs::symlink(temp.path(), &target_parent_alias).unwrap();

    assert!(matches!(
        storage.copy_directory(&source, &source_alias),
        Err(RelocationError::Inconsistent(_))
    ));
    assert!(matches!(
        storage.copy_directory(&source, &target_parent_alias.join("source/nested")),
        Err(RelocationError::Inconsistent(_))
    ));
    assert!(!source.join("nested").exists());
}

#[test]
fn durable_remove_and_atomic_no_replace_are_inert_building_blocks() {
    let temp = tempfile::tempdir().unwrap();
    let storage = RelocationStorage::new(temp.path().into());
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    storage.create_directory(&source).unwrap();
    storage.create_directory(&target).unwrap();
    assert!(matches!(
        storage.publish_no_replace(&source, &target),
        Err(RelocationError::Collision(_))
    ));
    storage.remove_directory(&source).unwrap();
    storage.remove_directory(&source).unwrap();
}
