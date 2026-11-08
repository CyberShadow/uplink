// `embed_migrations!` reads the directory at compile time, which Cargo does
// not track by itself.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
