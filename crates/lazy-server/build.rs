fn main() {
    // `sqlx::migrate!` embeds the migration files; rebuild when they change.
    println!("cargo:rerun-if-changed=migrations");
}
