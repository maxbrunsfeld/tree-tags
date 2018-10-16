#[macro_use]
extern crate serde_derive;

mod index;
mod language_registry;

use clap::{App, Arg, SubCommand};
use tree_sitter::Point;

fn main() -> index::Result<()> {
    let matches = App::new("Tree-tags")
        .version("0.1")
        .author("Max Brunsfeld <maxbrunsfeld@gmail.com>")
        .about("Indexes code")
        .subcommand(
            SubCommand::with_name("index")
                .about("Index a directory of source code")
                .arg(Arg::with_name("path").index(1)),
        ).subcommand(
            SubCommand::with_name("clear-index")
                .about("Clear the index for a directory of source code")
                .arg(Arg::with_name("path").index(1)),
        ).subcommand(
            SubCommand::with_name("find-definition")
                .about("Find the definition of a symbol")
                .arg(Arg::with_name("path").index(1).required(true))
                .arg(Arg::with_name("line").index(2).required(true))
                .arg(Arg::with_name("column").index(3).required(true)),
        ).subcommand(
            SubCommand::with_name("find-usages")
                .about("Find usages of a symbol")
                .arg(Arg::with_name("path").index(1).required(true))
                .arg(Arg::with_name("line").index(2).required(true))
                .arg(Arg::with_name("column").index(3).required(true)),
        ).get_matches();

    let pwd = std::env::current_dir()?;
    let config_dir = dirs::home_dir().unwrap().join(".config/tree-tags");
    let mut index = index::Index::new(config_dir);

    if let Some(matches) = matches.subcommand_matches("index") {
        index.index_path(pwd.join(matches.value_of("path").unwrap()).canonicalize()?)?;
    } else if let Some(matches) = matches.subcommand_matches("find-definition") {
        let path = pwd
            .join(matches.value_of("path").expect("Missing path"))
            .canonicalize()?;
        let line_arg = matches.value_of("line").expect("Missing line");
        let column_arg = matches.value_of("column").expect("Missing column");
        let position = Point {
            row: u32::from_str_radix(line_arg, 10).expect("Invalid row"),
            column: u32::from_str_radix(column_arg, 10).expect("Invalid column"),
        };
        for (path, position, length) in index.find_definition(path, position)? {
            println!(
                "{} {} {} {}",
                path.display(),
                position.row,
                position.column,
                length
            );
        }
    } else {
        eprintln!("Unknown command");
    }

    Ok(())
}
