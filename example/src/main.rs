//! A small interactive contact-book CLI demonstrating the whole `kladde`
//! stack: a `#[derive(Persistable)]` struct (`Contact`) and enum
//! (`PhoneNumber`), nested inside a `PersistedVec` and a
//! `PersistedHashMap`, mutated through generated `Guard`s and backed by
//! `kladde`'s `DefaultBackend`.
//!
//! v1 has no real file behind any of this (see `spec.md`'s "Crash
//! Consistency" note and `V1_QUESTIONS.md` questions 1 and 6) -- state
//! lives only for the duration of this process. Every mutating command
//! still gets recorded to an in-memory journal; the entry count printed
//! after each command is that journal growing, one entry per recorded
//! `Op`.

use kladde::Kladde;
use kladde_types::{Persistable, PersistedHashMap, PersistedVec};
use std::io::{self, Write};

#[derive(Persistable, Clone, serde::Serialize, serde::Deserialize, Debug)]
enum PhoneNumber {
    Mobile(String),
    Landline(String),
}

impl std::fmt::Display for PhoneNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PhoneNumber::Mobile(number) => write!(f, "mobile: {number}"),
            PhoneNumber::Landline(number) => write!(f, "landline: {number}"),
        }
    }
}

#[derive(Persistable, Clone, serde::Serialize, serde::Deserialize, Debug)]
struct Contact {
    email: String,
    phones: PersistedVec<PhoneNumber>,
}

#[derive(Persistable)]
struct AddressBook {
    contacts: PersistedHashMap<String, Contact>,
}

fn main() {
    println!("kladde contact book (v1 prototype -- in-memory only, see spec.md)");
    println!("commands: add <name> <email> | set-email <name> <email> | add-phone <name> mobile|landline <number> | show <name> | list | remove <name> | help | quit");

    let mut book = Kladde::new(AddressBook {
        contacts: PersistedHashMap::new(),
    });

    let stdin = io::stdin();
    loop {
        print!("> ");
        io::stdout().flush().ok();

        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            break; // EOF
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(&command) = words.first() else {
            continue;
        };

        match command {
            "quit" | "exit" => break,
            "help" => print_help(),
            "add" => match words.as_slice() {
                [_, name, email] => {
                    book.guard().contacts_mut().insert(
                        name.to_string(),
                        Contact {
                            email: email.to_string(),
                            phones: PersistedVec::new(),
                        },
                    );
                    println!("added {name} (journal entries: {})", journal_len(&book));
                }
                _ => println!("usage: add <name> <email>"),
            },
            "set-email" => match words.as_slice() {
                [_, name, email] => {
                    let found = {
                        let mut guard = book.guard();
                        match guard.contacts_mut().get_mut(&name.to_string()) {
                            Some(mut contact) => {
                                contact.email_mut().set(email.to_string());
                                true
                            }
                            None => false,
                        }
                    };
                    if found {
                        println!("updated email (journal entries: {})", journal_len(&book));
                    } else {
                        println!("no such contact: {name}");
                    }
                }
                _ => println!("usage: set-email <name> <email>"),
            },
            "add-phone" => match words.as_slice() {
                [_, name, kind, number] => {
                    let phone = match *kind {
                        "mobile" => PhoneNumber::Mobile(number.to_string()),
                        "landline" => PhoneNumber::Landline(number.to_string()),
                        other => {
                            println!("unknown phone kind {other:?}, expected mobile|landline");
                            continue;
                        }
                    };
                    let found = {
                        let mut guard = book.guard();
                        match guard.contacts_mut().get_mut(&name.to_string()) {
                            Some(mut contact) => {
                                contact.phones_mut().push(phone);
                                true
                            }
                            None => false,
                        }
                    };
                    if found {
                        println!(
                            "added phone number (journal entries: {})",
                            journal_len(&book)
                        );
                    } else {
                        println!("no such contact: {name}");
                    }
                }
                _ => println!("usage: add-phone <name> mobile|landline <number>"),
            },
            "remove" => match words.as_slice() {
                [_, name] => match book.guard().contacts_mut().remove(&name.to_string()) {
                    Some(_) => println!("removed {name} (journal entries: {})", journal_len(&book)),
                    None => println!("no such contact: {name}"),
                },
                _ => println!("usage: remove <name>"),
            },
            "show" => match words.as_slice() {
                [_, name] => match book.get().contacts.get(&name.to_string()) {
                    Some(contact) => {
                        println!("{name}: {}", contact.email);
                        for phone in contact.phones.iter() {
                            println!("  {phone}");
                        }
                    }
                    None => println!("no such contact: {name}"),
                },
                _ => println!("usage: show <name>"),
            },
            "list" => {
                if book.get().contacts.is_empty() {
                    println!("(no contacts yet)");
                } else {
                    for (name, contact) in book.get().contacts.iter() {
                        println!("{name}: {}", contact.email);
                    }
                }
            }
            other => println!("unknown command: {other:?} (try 'help')"),
        }
    }

    println!(
        "goodbye -- {} contact(s), {} op(s) journaled this session",
        book.get().contacts.len(),
        journal_len(&book)
    );
}

fn journal_len(book: &Kladde<AddressBook>) -> usize {
    book.backend().journal_entries().len()
}

fn print_help() {
    println!("add <name> <email>                       -- add a new contact");
    println!("set-email <name> <email>                  -- change a contact's email");
    println!("add-phone <name> mobile|landline <number> -- add a phone number");
    println!("show <name>                               -- show a contact's details");
    println!("list                                      -- list all contacts");
    println!("remove <name>                             -- remove a contact");
    println!(
        "quit | exit                               -- leave (state is not saved -- see spec.md)"
    );
}
