//! Writes `ui/src/bindings.ts` without launching the GUI:
//! `cargo run -p dntdatabase --example export_bindings`.

fn main() {
    let path = std::path::Path::new(dntdatabase::commands::BINDINGS_PATH);
    if let Err(e) = dntdatabase::commands::export_bindings(path) {
        eprintln!("export failed: {e:?}");
        std::process::exit(1);
    }
    println!("wrote {}", path.display());
}
