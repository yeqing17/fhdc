fn main() {
    let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp");
    println!("dir: {}", d.display());
    match std::fs::create_dir_all(&d) {
        Ok(_) => println!("create_dir_all ok"),
        Err(e) => println!("create_dir_all err: {e}"),
    }
    let f = d.join("probe.bin");
    match std::fs::write(&f, b"hi") {
        Ok(_) => println!("write ok"),
        Err(e) => println!("write err: {e}"),
    }
}
