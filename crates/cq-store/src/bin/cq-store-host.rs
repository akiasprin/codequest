fn main() {
    let args = std::env::args_os().collect::<Vec<_>>();
    let result = if args.len() == 3 && args[1] == "--store" {
        cq_store::runtime::serve(std::path::Path::new(&args[2]))
    } else {
        Err(cq_core::Error::new(
            "InvalidArguments",
            "cq-store-host --store PATH",
        ))
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
