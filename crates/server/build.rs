fn main() {
    // `sqlx::migrate!` reads the directory at compile time; cargo must rebuild the crate when a
    // migration file is added, which a proc macro cannot ask for on stable.
    println!("cargo:rerun-if-changed=../../db/migrations");
}
