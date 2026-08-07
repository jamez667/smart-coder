//! Print what `sc-win` makes of a project directory: its kind, the compile command it would
//! run, and (for Unity) the editor version it wants.
//!
//! Verification aid for spec 21's compile-and-check seam — run it against a real project to
//! confirm detection before wiring anything into the GUI.
//!
//!     cargo run -p sc-win --example detect_project -- <path>

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let root = std::path::PathBuf::from(&arg);

    let kind = sc_win::project::detect(&root);
    println!("path      : {}", root.display());
    println!("kind      : {}", kind.label());
    println!("compilable: {}", kind.compilable());

    if kind == sc_win::project::ProjectKind::Unity {
        match sc_win::project::unity_version(&root) {
            Some(v) => println!("unity ver : {v}"),
            None => println!("unity ver : (not stated)"),
        }
    }

    match sc_win::project::compile_command(&root, kind, None) {
        Ok(c) => println!("command   : {}", c.display()),
        Err(why) => println!("no command: {why}"),
    }
}
