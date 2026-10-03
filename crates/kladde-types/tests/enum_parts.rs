//! Mutating inside enum variants through `parts()`, with containers inside the
//! variants and enums inside containers, on a real `Kladde` that is reopened
//! from its bytes alone.

use kladde::{Kladde, MemoryStorage, Options, Persistable};
use kladde_types::{PersistableString, PersistableVec};

#[derive(Persistable)]
enum Node {
    Text(PersistableString),
    Group {
        label: PersistableString,
        children: PersistableVec<Node>,
    },
}

fn text(s: &str) -> Node {
    Node::Text(PersistableString::from(s))
}

fn reopened(storage: &MemoryStorage) -> Kladde<Node> {
    let image = MemoryStorage::from_image(storage.image());
    Kladde::open_in(Box::new(image), Options::default()).unwrap()
}

/// The tree as text: `label[child, child]` for a group, the string for text.
fn render(node: &Node) -> String {
    match node {
        Node::Text(s) => s.to_string(),
        Node::Group { label, children } => {
            let children: Vec<String> = children.iter().map(render).collect();
            format!("{label}[{}]", children.join(", "))
        }
    }
}

#[test]
fn nested_variants_are_mutated_in_place_and_survive_a_reopen() {
    let storage = MemoryStorage::new();
    let root = Node::Group {
        label: PersistableString::from("root"),
        children: PersistableVec::from_iter([
            text("a"),
            Node::Group {
                label: PersistableString::from("inner"),
                children: PersistableVec::from_iter([text("b")]),
            },
        ]),
    };
    let mut tree = Kladde::create_in(Box::new(storage.clone()), root, Options::default()).unwrap();

    {
        let mut guard = tree.guard();
        let NodeParts::Group {
            mut label,
            mut children,
        } = guard.parts()
        else {
            panic!("the root is a group");
        };
        label.push_str("!").unwrap();

        // A text child: edit its string.
        if let NodeParts::Text(mut s) = children.get_mut(0).unwrap().parts() {
            s.push_str("a").unwrap();
        }

        // A group child: push into its vector of children.
        let mut inner = children.get_mut(1).unwrap();
        if let NodeParts::Group { mut children, .. } = inner.parts() {
            children.push(text("c")).unwrap();
        }

        // Replace a whole child, switching its variant.
        children
            .get_mut(0)
            .unwrap()
            .set(Node::Group {
                label: PersistableString::from("new"),
                children: PersistableVec::new(),
            })
            .unwrap();
    }

    let expected = "root![new[], inner[b, c]]";
    assert_eq!(render(tree.get()), expected);
    assert_eq!(render(reopened(&storage).get()), expected);
    tree.flush().unwrap();
    assert_eq!(render(reopened(&storage).get()), expected);
}
