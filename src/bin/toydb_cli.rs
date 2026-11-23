use std::env;
use std::io::{self, Write};
use std::path::PathBuf;

use toydb::Db;

fn main() {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./toydb-data"));

    let mut db = match Db::open(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("failed to open database at {:?}: {}", path, e);
            std::process::exit(1);
        }
    };

    println!("toydb REPL -- data dir: {:?}", path);
    println!("commands: SET k v | GET k | DEL k | SCAN prefix | EXIT");

    let stdin = io::stdin();
    loop {
        print!("toydb> ");
        if io::stdout().flush().is_err() {
            break;
        }

        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => break, // EOF (e.g. piped input ran out)
            Ok(_) => {}
            Err(e) => {
                eprintln!("read error: {}", e);
                break;
            }
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let (command, arguments) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let arguments = arguments.trim_start();
        let cmd = command.to_ascii_uppercase();
        let (key, value) = arguments
            .split_once(char::is_whitespace)
            .map_or((arguments, None), |(key, rest)| (key, Some(rest.trim_start())));
        let key = (!key.is_empty()).then_some(key);

        match cmd.as_str() {
            "SET" => match (key, value) {
                (Some(k), Some(v)) => match db.put(k.as_bytes(), v.as_bytes()) {
                    Ok(()) => println!("OK"),
                    Err(e) => println!("ERR {}", e),
                },
                _ => println!("usage: SET <key> <value>"),
            },
            "GET" => match key {
                Some(k) => match db.get(k.as_bytes()) {
                    Ok(Some(v)) => println!("{}", String::from_utf8_lossy(&v)),
                    Ok(None) => println!("(nil)"),
                    Err(e) => println!("ERR {}", e),
                },
                None => println!("usage: GET <key>"),
            },
            "DEL" => match key {
                Some(k) => match db.delete(k.as_bytes()) {
                    Ok(()) => println!("OK"),
                    Err(e) => println!("ERR {}", e),
                },
                None => println!("usage: DEL <key>"),
            },
            "SCAN" => {
                let prefix = key.unwrap_or("");
                match db.scan(prefix.as_bytes()) {
                    Ok(iter) => {
                        let mut count = 0;
                        for (k, v) in iter {
                            println!(
                                "{} = {}",
                                String::from_utf8_lossy(&k),
                                String::from_utf8_lossy(&v)
                            );
                            count += 1;
                        }
                        if count == 0 {
                            println!("(empty)");
                        }
                    }
                    Err(e) => println!("ERR {}", e),
                }
            }
            "EXIT" | "QUIT" => break,
            other => println!("unknown command: {} (try SET/GET/DEL/SCAN/EXIT)", other),
        }
    }
}
