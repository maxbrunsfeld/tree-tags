#[macro_use]
extern crate serde_derive;

mod crawler;
mod language_registry;
mod store;

use std::io;
use std::path::PathBuf;
use clap::{App, Arg, SubCommand};
use tree_sitter::Point;

fn main() -> crawler::Result<()> {
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

    let config_path = dirs::home_dir().unwrap().join(".config/tree-tags");
    let db_path = config_path.join("db.sqlite");
    let parsers_path = config_path.join("parsers");
    let compiled_parsers_path = config_path.join("parsers-compiled");

    let mut store = store::Store::new(db_path)?;
    let mut language_registry = language_registry::LanguageRegistry::new(
        compiled_parsers_path,
        vec![parsers_path]
    );

    if let Some(matches) = matches.subcommand_matches("index") {
        language_registry.load_parsers()?;
        let mut crawler = crawler::DirCrawler::new(store, language_registry);
        crawler.crawl_path(get_path_arg(matches.value_of("path").unwrap())?)?;
        return Ok(());
    }

    if let Some(matches) = matches.subcommand_matches("clear-index") {
        store.delete_files(&get_path_arg(matches.value_of("path").unwrap())?)?;
        return Ok(());
    }

    if let Some(matches) = matches.subcommand_matches("find-definition") {
        let path = get_path_arg(matches.value_of("path").expect("Missing path"))?;
        let line_arg = matches.value_of("line").expect("Missing line");
        let column_arg = matches.value_of("column").expect("Missing column");
        let position = Point {
            row: u32::from_str_radix(line_arg, 10).expect("Invalid row"),
            column: u32::from_str_radix(column_arg, 10).expect("Invalid column"),
        };
        for (path, position, length) in store.find_definition(&path, position)? {
            println!(
                "{} {} {} {}",
                path.display(),
                position.row,
                position.column,
                length
            );
        }
        return Ok(());
    }

    eprintln!("Unknown command");
    Ok(())
}

fn get_path_arg(arg: &str) -> io::Result<PathBuf> {
    std::env::current_dir().and_then(|cwd| cwd.join(arg).canonicalize())
}
