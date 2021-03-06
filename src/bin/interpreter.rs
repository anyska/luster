extern crate failure;
extern crate luster;

use std::env;
use std::fs::File;
use std::io::Read;

use failure::{err_msg, Error};

use luster::state::Lua;

fn main() -> Result<(), Error> {
    let mut args = env::args();
    args.next();
    let mut file = File::open(args.next()
        .ok_or_else(|| err_msg("no file argument given"))?)?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;

    let mut lua = Lua::load(&contents)?;
    while !lua.run(Some(1024)) {}

    lua.visit_results(|results| {
        println!("results: {:?}", results.unwrap());
    });

    Ok(())
}
