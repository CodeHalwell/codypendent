// sqlx::migrate! tracks existing SQL files, but adding a migration must also
// invalidate this crate so its embedded schema cannot lag behind the workspace.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
