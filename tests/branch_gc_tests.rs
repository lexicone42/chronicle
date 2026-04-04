//! Tests for branch GC utilities — targeting mutation testing gaps in
//! is_visible, get_orphaned_branches, get_stale_branches,
//! get_child_branches, delete_branch_with_reparent, and gc.
//!
//! These test BranchManager directly since the GC methods aren't
//! exposed on the Store API.

use chronicle::{BranchGcOptions, BranchManager, Sequence, Timestamp};
use tempfile::TempDir;

fn test_manager(dir: &TempDir) -> BranchManager {
    BranchManager::new(dir.path().join("branches.bin")).unwrap()
}

// ===========================================================================
// is_visible
// ===========================================================================

#[test]
fn test_is_visible_on_main() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    // Advance main branch head
    mgr.update_head(mgr.current_branch().unwrap().id, Sequence(5)).unwrap();

    assert!(mgr.is_visible("main", Sequence(1)).unwrap());
    assert!(mgr.is_visible("main", Sequence(5)).unwrap());
    assert!(!mgr.is_visible("main", Sequence(6)).unwrap());
}

#[test]
fn test_is_visible_beyond_head_false() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    assert!(!mgr.is_visible("main", Sequence(999)).unwrap());
}

#[test]
fn test_is_visible_nonexistent_branch() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    assert!(mgr.is_visible("ghost", Sequence(1)).is_err());
}

#[test]
fn test_is_visible_child_branch() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    mgr.update_head(mgr.current_branch().unwrap().id, Sequence(5)).unwrap();
    mgr.create_branch("child", None).unwrap();
    mgr.update_head(
        mgr.get_branch("child").unwrap().id,
        Sequence(8),
    ).unwrap();

    // Child sees up to its head
    assert!(mgr.is_visible("child", Sequence(3)).unwrap());
    assert!(mgr.is_visible("child", Sequence(8)).unwrap());
    assert!(!mgr.is_visible("child", Sequence(9)).unwrap());
}

// ===========================================================================
// get_orphaned_branches
// ===========================================================================

#[test]
fn test_no_orphans_initially() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    mgr.create_branch("child", None).unwrap();
    assert!(mgr.get_orphaned_branches().is_empty());
}

// ===========================================================================
// get_stale_branches
// ===========================================================================

#[test]
fn test_stale_branches_far_future() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    mgr.create_branch("old1", None).unwrap();
    mgr.create_branch("old2", None).unwrap();

    let far_future = (Timestamp::now().0 as u64) + 1_000_000;
    let stale = mgr.get_stale_branches(far_future);

    let names: Vec<&str> = stale.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"old1"));
    assert!(names.contains(&"old2"));
    assert!(!names.contains(&"main"), "Main should never be stale");
}

#[test]
fn test_stale_branches_past_timestamp() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    mgr.create_branch("fresh", None).unwrap();
    let stale = mgr.get_stale_branches(0);
    assert!(stale.is_empty(), "Nothing stale with timestamp=0");
}

// ===========================================================================
// get_child_branches
// ===========================================================================

#[test]
fn test_get_child_branches() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    mgr.create_branch("child1", None).unwrap();
    mgr.create_branch("child2", None).unwrap();
    mgr.create_branch("grandchild", Some("child1")).unwrap();

    let main_id = mgr.current_branch().unwrap().id;
    let children = mgr.get_child_branches(main_id);
    let names: Vec<&str> = children.iter().map(|b| b.name.as_str()).collect();

    assert_eq!(names.len(), 2);
    assert!(names.contains(&"child1"));
    assert!(names.contains(&"child2"));
    assert!(!names.contains(&"grandchild"));
}

#[test]
fn test_get_child_branches_leaf() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    mgr.create_branch("leaf", None).unwrap();
    let leaf = mgr.get_branch("leaf").unwrap();
    assert!(mgr.get_child_branches(leaf.id).is_empty());
}

// ===========================================================================
// delete_branch_with_reparent
// ===========================================================================

#[test]
fn test_delete_with_reparent() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    mgr.create_branch("parent", None).unwrap();
    mgr.create_branch("child", Some("parent")).unwrap();

    let reparented = mgr.delete_branch_with_reparent("parent", Some("main")).unwrap();
    assert_eq!(reparented, 1);

    // child should still exist
    assert!(mgr.get_branch("child").is_some());
    // parent should be gone
    assert!(mgr.get_branch("parent").is_none());
}

#[test]
fn test_delete_with_reparent_orphans() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    mgr.create_branch("doomed", None).unwrap();
    mgr.create_branch("orphan1", Some("doomed")).unwrap();
    mgr.create_branch("orphan2", Some("doomed")).unwrap();

    let reparented = mgr.delete_branch_with_reparent("doomed", None).unwrap();
    assert_eq!(reparented, 2);
}

// ===========================================================================
// gc
// ===========================================================================

#[test]
fn test_gc_deletes_empty_branches() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    // Create empty branches
    mgr.create_branch("empty1", None).unwrap();
    mgr.create_branch("empty2", None).unwrap();

    // Create a "non-empty" branch (advance its head past branch point)
    mgr.create_branch("active", None).unwrap();
    let active = mgr.get_branch("active").unwrap();
    mgr.update_head(active.id, Sequence(active.head.0 + 1)).unwrap();

    let result = mgr.gc(BranchGcOptions {
        delete_empty: true,
        ..Default::default()
    }).unwrap();

    assert!(result.deleted.contains(&"empty1".to_string()));
    assert!(result.deleted.contains(&"empty2".to_string()));
    assert!(!result.deleted.contains(&"active".to_string()));
    assert!(!result.deleted.contains(&"main".to_string()));
}

#[test]
fn test_gc_with_name_patterns() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    mgr.create_branch("temp/a", None).unwrap();
    mgr.create_branch("temp/b", None).unwrap();
    mgr.create_branch("keep/c", None).unwrap();

    let result = mgr.gc(BranchGcOptions {
        delete_empty: true,
        name_patterns: Some(vec!["temp/".to_string()]),
        ..Default::default()
    }).unwrap();

    assert_eq!(result.deleted.len(), 2);
    assert!(!result.deleted.contains(&"keep/c".to_string()));
}

#[test]
fn test_gc_stale() {
    let dir = TempDir::new().unwrap();
    let mgr = test_manager(&dir);

    mgr.create_branch("old", None).unwrap();

    let far_future = (Timestamp::now().0 as u64) + 1_000_000;
    let result = mgr.gc(BranchGcOptions {
        delete_stale_older_than: Some(far_future),
        ..Default::default()
    }).unwrap();

    assert!(result.deleted.contains(&"old".to_string()));
}
