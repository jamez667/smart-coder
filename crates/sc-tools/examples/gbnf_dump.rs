fn main() {
    print!(
        "{}",
        sc_tools::registry_gbnf(&sc_tools::read_only_registry())
    );
}
