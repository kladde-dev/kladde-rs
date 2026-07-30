//! A small interactive contact-book CLI demonstrating the whole `kladde`
//! stack: a `#[derive(Persistable)]` struct (`Contact`) and enum
//! (`PhoneNumber`), nested inside a `PersistableVec` and a
//! `PersistableHashMap`, mutated through generated `Guard`s and backed by
//! `kladde`'s `DefaultBackend`. Run with `cargo run -p kladde-types
//! --example phonebook`.
//!
//! v1 has no real file behind any of this (see `spec.md`'s "Crash
//! Consistency" note) -- state lives only for the duration of this
//! process. Every mutating command still gets recorded to an in-memory
//! journal of microoperations; the count printed after each command is
//! that journal growing, and `flush` (or quitting) drains it by replaying
//! everything into the (also in-memory, mock) allocator.

use kladde::Kladde;
use kladde_types::{Persistable, PersistableHashMap, PersistableString, PersistableVec};
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

fn main() {
    println!("kladde contact book (v1 prototype -- in-memory only, see spec.md)");
    println!(
        "commands: add <name> <email> | set-email <name> <email> | add-phone <name> mobile|landline <number> \
         | show <name> | list | remove <name> | flush | help | quit"
    );

    let mut book = Kladde::new(AddressBook {
        contacts: PersistableHashMap::new(),
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
            "flush" => {
                book.flush();
                println!("flushed (journal entries: {})", journal_len(&book));
            }
            "add" => match words.as_slice() {
                [_, name, email] => {
                    book.guard().contacts_mut().insert(
                        PersistableString::from(*name),
                        Contact {
                            email: PersistableString::from(*email),
                            phones: PersistableVec::new(),
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
                        match guard
                            .contacts_mut()
                            .get_mut(&PersistableString::from(*name))
                        {
                            Some(mut contact) => {
                                contact.email_mut().set(*email);
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
                        "mobile" => PhoneNumber::Mobile(PersistableString::from(*number)),
                        "landline" => PhoneNumber::Landline(PersistableString::from(*number)),
                        other => {
                            println!("unknown phone kind {other:?}, expected mobile|landline");
                            continue;
                        }
                    };
                    let found = {
                        let mut guard = book.guard();
                        match guard
                            .contacts_mut()
                            .get_mut(&PersistableString::from(*name))
                        {
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
            "set-email-and-add-phone" => match words.as_slice() {
                [_, name, email, kind, number] => {
                    let phone = match *kind {
                        "mobile" => PhoneNumber::Mobile(PersistableString::from(*number)),
                        "landline" => PhoneNumber::Landline(PersistableString::from(*number)),
                        other => {
                            println!("unknown phone kind {other:?}, expected mobile|landline");
                            continue;
                        }
                    };
                    let found = {
                        let mut guard = book.guard();
                        match guard
                            .contacts_mut()
                            .get_mut(&PersistableString::from(*name))
                        {
                            Some(mut contact) => {
                                // Illustrates destructuring the guards for all
                                // fields at once via `.parts()`: `email` and
                                // `phones` are two field guards, live together.
                                let ContactParts {
                                    email: mut email_guard,
                                    mut phones,
                                } = contact.parts();
                                email_guard.set(*email);
                                phones.push(phone);
                                true
                            }
                            None => false,
                        }
                    };
                    if found {
                        println!(
                            "updated email and added phone (journal entries: {})",
                            journal_len(&book)
                        );
                    } else {
                        println!("no such contact: {name}");
                    }
                }
                _ => println!(
                    "usage: set-email-and-add-phone <name> <email> mobile|landline <number>"
                ),
            },
            "remove" => match words.as_slice() {
                [_, name] => match book
                    .guard()
                    .contacts_mut()
                    .remove(&PersistableString::from(*name))
                {
                    Some(_) => println!("removed {name} (journal entries: {})", journal_len(&book)),
                    None => println!("no such contact: {name}"),
                },
                _ => println!("usage: remove <name>"),
            },
            "show" => match words.as_slice() {
                [_, name] => match book.get().contacts.get(&PersistableString::from(*name)) {
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

    book.flush();
    println!(
        "goodbye -- {} contact(s), flushed and 0 op(s) left in the journal",
        book.get().contacts.len(),
    );
}

fn journal_len(book: &Kladde<AddressBook>) -> usize {
    book.backend().journal_len()
}

fn print_help() {
    println!("add <name> <email>                       -- add a new contact");
    println!("set-email <name> <email>                  -- change a contact's email");
    println!("add-phone <name> mobile|landline <number> -- add a phone number");
    println!("set-email-and-add-phone <name> <email> mobile|landline <number>");
    println!("                                          -- both at once, via .parts()");
    println!("show <name>                               -- show a contact's details");
    println!("list                                      -- list all contacts");
    println!("remove <name>                             -- remove a contact");
    println!("flush                                     -- replay the journal into the (in-memory) snapshot");
    println!(
        "quit | exit                               -- flush and leave (state is not saved to a real file -- see spec.md)"
    );
}
