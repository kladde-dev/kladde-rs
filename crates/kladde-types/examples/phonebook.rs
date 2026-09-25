//! A small interactive contact book on a real file: a `#[derive(Persistable)]`
//! struct and enum, nested in a `PersistableVec` and a `PersistableHashMap`,
//! mutated through the generated guards.
//!
//! Run with `cargo run -p kladde-types --example phonebook -- [path]`. Every
//! command that changes the book is in the file when it has printed its
//! answer: kill the process at any point, run it again, and the book is there.

use kladde::{Kladde, Persistable};
use kladde_types::{PersistableHashMap, PersistableString, PersistableVec};
use std::io::{self, Write};

#[derive(Persistable, Debug)]
enum PhoneNumber {
    Mobile(PersistableString),
    Landline(PersistableString),
}

impl std::fmt::Display for PhoneNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PhoneNumber::Mobile(number) => write!(f, "mobile: {number}"),
            PhoneNumber::Landline(number) => write!(f, "landline: {number}"),
        }
    }
}

#[derive(Persistable, Debug)]
struct Contact {
    email: PersistableString,
    phones: PersistableVec<PhoneNumber>,
}

#[derive(Persistable)]
struct AddressBook {
    contacts: PersistableHashMap<PersistableString, Contact>,
}

fn main() -> kladde::Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("phonebook.kladde"));
    let mut book = if path.exists() {
        Kladde::open(&path)?
    } else {
        Kladde::create(
            &path,
            AddressBook {
                contacts: PersistableHashMap::new(),
            },
        )?
    };
    println!("kladde contact book at {}", path.display());
    print_help();

    let stdin = io::stdin();
    loop {
        print!("> ");
        io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(&command) = words.first() else {
            continue;
        };
        match (command, &words[1..]) {
            ("quit" | "exit", _) => break,
            ("help", _) => print_help(),
            ("add", [name, email]) => {
                let contact = Contact {
                    email: PersistableString::from(*email),
                    phones: PersistableVec::new(),
                };
                book.guard()
                    .contacts_mut()
                    .insert(PersistableString::from(*name), contact)?;
                println!("added {name}");
            }
            ("set-email", [name, email]) => {
                let mut guard = book.guard();
                let mut contacts = guard.contacts_mut();
                match contacts.get_mut(&PersistableString::from(*name)) {
                    Some(mut contact) => {
                        contact.email_mut().set(*email)?;
                        println!("updated {name}");
                    }
                    None => println!("no such contact: {name}"),
                }
            }
            ("add-phone", [name, kind, number]) => {
                let phone = match *kind {
                    "mobile" => PhoneNumber::Mobile(PersistableString::from(*number)),
                    "landline" => PhoneNumber::Landline(PersistableString::from(*number)),
                    other => {
                        println!("unknown phone kind {other:?}, expected mobile|landline");
                        continue;
                    }
                };
                let mut guard = book.guard();
                let mut contacts = guard.contacts_mut();
                match contacts.get_mut(&PersistableString::from(*name)) {
                    Some(mut contact) => {
                        // Two field guards at once, through `parts()`.
                        let ContactParts { mut phones, .. } = contact.parts();
                        phones.push(phone)?;
                        println!("added a phone number to {name}");
                    }
                    None => println!("no such contact: {name}"),
                }
            }
            ("remove", [name]) => {
                if book
                    .guard()
                    .contacts_mut()
                    .delete(&PersistableString::from(*name))?
                {
                    println!("removed {name}");
                } else {
                    println!("no such contact: {name}");
                }
            }
            ("show", [name]) => match book.get().contacts.get(&PersistableString::from(*name)) {
                Some(contact) => {
                    println!("{name}: {}", contact.email);
                    for phone in contact.phones.iter() {
                        println!("  {phone}");
                    }
                }
                None => println!("no such contact: {name}"),
            },
            ("list", _) if book.get().contacts.is_empty() => println!("(no contacts yet)"),
            ("list", _) => {
                for (name, contact) in book.get().contacts.iter() {
                    println!("{name}: {}", contact.email);
                }
            }
            ("stats", _) => {
                let s = book.stats();
                println!(
                    "{} pages, {} allocations, {} flushes this session",
                    s.file_pages, s.allocations, s.flushes
                );
            }
            (other, _) => println!("unknown command or arguments: {other:?} (try 'help')"),
        }
    }
    let contacts = book.get().contacts.len();
    book.close()?;
    println!("goodbye -- {contacts} contact(s)");
    Ok(())
}

fn print_help() {
    println!("add <name> <email>                        -- add a contact");
    println!("set-email <name> <email>                  -- change a contact's email");
    println!("add-phone <name> mobile|landline <number> -- add a phone number");
    println!("show <name>                               -- show a contact");
    println!("list                                      -- list all contacts");
    println!("remove <name>                             -- remove a contact");
    println!("stats                                     -- what the file looks like");
    println!("quit | exit                               -- close the file and leave");
}
