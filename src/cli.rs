pub enum Command {
    /// No arguments: create new session or attach to last.
    Default,
    /// wynd new [-s name]
    New { name: Option<String> },
    /// wynd attach [-t name]
    Attach { target: Option<String> },
    /// wynd ls
    List,
    /// wynd kill [-t name]
    Kill { target: Option<String> },
}

pub fn parse_args() -> Command {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        return Command::Default;
    }

    match args[0].as_str() {
        "new" => {
            let name = parse_flag(&args[1..], "-s");
            Command::New { name }
        }
        "attach" | "a" => {
            let target = parse_flag(&args[1..], "-t");
            Command::Attach { target }
        }
        "ls" | "list" => Command::List,
        "kill" => {
            let target = parse_flag(&args[1..], "-t");
            Command::Kill { target }
        }
        _ => {
            eprintln!("unknown command: {}", args[0]);
            eprintln!("usage: wynd [new [-s name] | attach [-t name] | ls | kill [-t name]]");
            std::process::exit(1);
        }
    }
}

fn parse_flag(args: &[String], flag: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().cloned();
        }
        // Allow bare argument without flag.
        if !arg.starts_with('-') {
            return Some(arg.clone());
        }
    }
    None
}
