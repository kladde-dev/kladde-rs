//! A small interactive contact-book CLI demonstrating the whole `kladde`
//! stack: a `#[derive(Persistable)]` struct (`Contact`) nesting a
//! `PersistedVec`/`PersistedHashMap`, mutated through generated `Guard`s
//! and backed by `kladde`'s `DefaultBackend`.
//!
//! v1 has no real file behind any of this (see `spec.md`'s "Crash
//! Consistency" note) -- state lives only for the duration of this
//! process. Every mutating command still gets recorded to an in-memory
//! journal of microoperations; the count printed after each command is
//! that journal growing, and `flush` (or quitting) drains it by replaying
//! everything into the (also in-memory, mock) allocator.

use kladde::Kladde;
use kladde_types::{Persistable, Persisted, PersistedHashMap, PersistedString, PersistedVec};
use std::io::{self, Write};

// TODO(enum redesign): `#[derive(Persistable)]` doesn't support enums yet
// (see spec.md's Future Work) -- once the inline per-variant layout lands,
// drop the `serde`/`Default` derives here and the `Persisted<...>`
// wrapping around `Contact::phones` below, and derive `Persistable`
// directly on `PhoneNumber` again.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
enum PhoneNumber {
    Mobile(String),
    Landline(String),
}

impl Default for PhoneNumber {
    fn default() -> Self {
        PhoneNumber::Mobile(String::new())
    }
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
    email: PersistedString,
    phones: PersistedVec<Persisted<PhoneNumber>>,
}

#[derive(Persistable)]
struct AddressBook {
    contacts: PersistedHashMap<PersistedString, Contact>,
}

fn main() {
    println!("kladde contact book (v1 prototype -- in-memory only, see spec.md)");
    println!(
        "commands: add <name> <email> | set-email <name> <email> | add-phone <name> mobile|landline <number> \
         | show <name> | list | remove <name> | flush | help | quit"
    );

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
            "flush" => {
                book.flush();
                println!("flushed (journal entries: {})", journal_len(&book));
            }
            "add" => match words.as_slice() {
                [_, name, email] => {
                    book.guard().contacts_mut().insert(
                        PersistedString::from(*name),
                        Contact {
                            email: PersistedString::from(*email),
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
                        match guard.contacts_mut().get_mut(&PersistedString::from(*name)) {
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
                        "mobile" => PhoneNumber::Mobile(number.to_string()),
                        "landline" => PhoneNumber::Landline(number.to_string()),
                        other => {
                            println!("unknown phone kind {other:?}, expected mobile|landline");
                            continue;
                        }
                    };
                    // Constructed before `book.guard()` so this immutable
                    // borrow of `book` ends before the mutable one begins.
                    let phone = Persisted::new(phone, book.backend());
                    let found = {
                        let mut guard = book.guard();
                        match guard.contacts_mut().get_mut(&PersistedString::from(*name)) {
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
                [_, name] => match book
                    .guard()
                    .contacts_mut()
                    .remove(&PersistedString::from(*name))
                {
                    Some(_) => println!("removed {name} (journal entries: {})", journal_len(&book)),
                    None => println!("no such contact: {name}"),
                },
                _ => println!("usage: remove <name>"),
            },
            "show" => match words.as_slice() {
                [_, name] => match book.get().contacts.get(&PersistedString::from(*name)) {
                    Some(contact) => {
                        println!("{name}: {}", contact.email);
                        for phone in contact.phones.iter() {
                            println!("  {}", **phone);
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
    println!("show <name>                               -- show a contact's details");
    println!("list                                      -- list all contacts");
    println!("remove <name>                             -- remove a contact");
    println!("flush                                     -- replay the journal into the (in-memory) snapshot");
    println!(
        "quit | exit                               -- flush and leave (state is not saved to a real file -- see spec.md)"
    );
}
