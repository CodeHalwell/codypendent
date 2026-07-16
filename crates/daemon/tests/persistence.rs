//! Phase 0 exit criterion: daemon restart preserves its instance database.

use codypendent_daemon::{db, instance};

#[tokio::test]
async fn instance_identity_survives_restart() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let db_path = tmp.path().join("codypendent.db");

    let pool1 = db::open_database(&db_path)
        .await
        .expect("open db first time");
    let boot1 = instance::record_boot(&pool1).await.expect("first boot");
    assert_eq!(boot1.boot_count, 1);
    pool1.close().await;

    let pool2 = db::open_database(&db_path)
        .await
        .expect("open db second time");
    let boot2 = instance::record_boot(&pool2).await.expect("second boot");

    assert_eq!(
        boot2.instance_id, boot1.instance_id,
        "instance identity must persist"
    );
    assert_eq!(
        boot2.boot_count, 2,
        "boot count must increment across restarts"
    );
}
