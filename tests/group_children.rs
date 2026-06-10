//! End-to-end EA IFF 85 §5 LIST/CAT child walking.
//!
//! Builds the §5 worked example —
//! `LIST { PROP TEXT { FONT } FORM TEXT { FONT CHRS } FORM TEXT { CHRS } }`
//! — as a complete on-disk file image, probes the top-level envelope
//! with [`chunk::probe_top_level_group`], and walks the children with
//! [`chunk::parse_group_children`], resolving the shared FONT property
//! through [`chunk::prop_for_form_type`].

use std::io::Cursor;

use oxideav_iff::chunk::{
    self, parse_group_children, probe_top_level_group, prop_for_form_type, GroupChild, GroupKind,
};

/// `id + ckSize + body (+ pad)` — one wire chunk.
fn wire_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len() + 1);
    v.extend_from_slice(id);
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend_from_slice(body);
    if body.len() & 1 == 1 {
        v.push(0);
    }
    v
}

/// The §5 worked example as a full file image (top-level LIST).
fn worked_example() -> Vec<u8> {
    let prop = wire_chunk(
        b"PROP",
        &[b"TEXT".to_vec(), wire_chunk(b"FONT", b"TimesRoman")].concat(),
    );
    let form1 = wire_chunk(
        b"FORM",
        &[
            b"TEXT".to_vec(),
            wire_chunk(b"FONT", b"Helvetica"),
            wire_chunk(b"CHRS", b"Hello "),
        ]
        .concat(),
    );
    let form2 = wire_chunk(
        b"FORM",
        &[b"TEXT".to_vec(), wire_chunk(b"CHRS", b"there.")].concat(),
    );
    wire_chunk(b"LIST", &[b"TEXT".to_vec(), prop, form1, form2].concat())
}

/// Read every `(id, body)` pair out of a flat chunk stream.
fn read_all_chunks(mut bytes: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let mut out = Vec::new();
    let total = bytes.len() as u64;
    let mut cur = Cursor::new(&mut bytes);
    while let Some(h) = chunk::read_chunk_header(&mut cur).unwrap() {
        let body = chunk::read_body(&mut cur, &h).unwrap();
        out.push((h.id, body));
        if cur.position() >= total {
            break;
        }
        chunk::skip_pad(&mut cur, &h).unwrap();
    }
    out
}

#[test]
fn worked_example_list_walks_end_to_end() {
    let file = worked_example();

    // §6: an IFF file is a single FORM/LIST/CAT chunk at offset 0.
    let top = probe_top_level_group(&file).unwrap().unwrap();
    assert_eq!(top.kind, GroupKind::List);
    assert_eq!(&top.inner_type, b"TEXT");
    assert_eq!(top.declared_total_len(), file.len() as u64);

    // Children start after the 12-byte envelope, bounded by ckSize.
    let children_bytes = &file[12..top.declared_total_len() as usize];
    let kids = parse_group_children(top.kind, children_bytes).unwrap();
    assert_eq!(kids.len(), 3);

    // §5 ordering: the PROP leads, the two FORMs follow.
    assert!(kids[0].is_prop());
    assert!(!kids[1].is_prop());
    assert!(!kids[2].is_prop());

    // §5: "Here are the shared properties for FORM type TEXT."
    let shared = prop_for_form_type(&kids, *b"TEXT").expect("PROP TEXT present");
    let props = read_all_chunks(shared);
    assert_eq!(props.len(), 1);
    assert_eq!(&props[0].0, b"FONT");
    assert_eq!(props[0].1, b"TimesRoman");

    // First FORM overrides FONT locally (§5 ¶ "Individual FORMs can
    // override the shared settings"): its own chunk list carries a
    // FONT ahead of the CHRS.
    let GroupChild::Group {
        kind,
        inner_type,
        body,
    } = kids[1]
    else {
        panic!("expected nested FORM");
    };
    assert_eq!(kind, GroupKind::Form);
    assert_eq!(&inner_type, b"TEXT");
    let local = read_all_chunks(body);
    assert_eq!(&local[0].0, b"FONT");
    assert_eq!(local[0].1, b"Helvetica");
    assert_eq!(&local[1].0, b"CHRS");
    assert_eq!(local[1].1, b"Hello ");

    // Second FORM carries no local FONT — §5's example resolves it
    // through the shared PROP ("uses font TimesRoman").
    let GroupChild::Group { body, .. } = kids[2] else {
        panic!("expected nested FORM");
    };
    let local = read_all_chunks(body);
    assert_eq!(local.len(), 1);
    assert_eq!(&local[0].0, b"CHRS");
    assert_eq!(local[0].1, b"there.");
}

#[test]
fn cat_of_forms_walks_end_to_end() {
    // §5 Group CAT: "A CAT is just an untyped group of data objects"
    // with the blank "JJJJ" contents ID for heterogeneous content.
    let form1 = wire_chunk(
        b"FORM",
        &[b"ILBM".to_vec(), wire_chunk(b"BMHD", &[0; 20])].concat(),
    );
    let form2 = wire_chunk(
        b"FORM",
        &[b"8SVX".to_vec(), wire_chunk(b"VHDR", &[0; 20])].concat(),
    );
    let file = wire_chunk(b"CAT ", &[b"JJJJ".to_vec(), form1, form2].concat());

    let top = probe_top_level_group(&file).unwrap().unwrap();
    assert_eq!(top.kind, GroupKind::Cat);
    assert_eq!(top.inner_type_str(), "JJJJ");

    let kids = parse_group_children(top.kind, &file[12..]).unwrap();
    assert_eq!(kids.len(), 2);
    assert_eq!(kids[0].inner_type(), *b"ILBM");
    assert_eq!(kids[1].inner_type(), *b"8SVX");
    // A CAT shares nothing — no PROP can exist here.
    assert!(prop_for_form_type(&kids, *b"ILBM").is_none());
}

#[test]
fn nested_list_inside_cat_walks_recursively() {
    // CAT ::= "CAT " #{ ContentsType (FORM | LIST | CAT)* } — a LIST
    // is a legal CAT child; its own children walk with a second
    // parse_group_children call.
    let inner_form = wire_chunk(
        b"FORM",
        &[b"TEXT".to_vec(), wire_chunk(b"CHRS", b"hi")].concat(),
    );
    let inner_list = wire_chunk(b"LIST", &[b"TEXT".to_vec(), inner_form].concat());
    let file = wire_chunk(b"CAT ", &[b"TEXT".to_vec(), inner_list].concat());

    let top = probe_top_level_group(&file).unwrap().unwrap();
    let kids = parse_group_children(top.kind, &file[12..]).unwrap();
    assert_eq!(kids.len(), 1);
    let GroupChild::Group {
        kind,
        inner_type,
        body,
    } = kids[0]
    else {
        panic!("expected nested LIST");
    };
    assert_eq!(kind, GroupKind::List);
    assert_eq!(&inner_type, b"TEXT");

    let inner_kids = parse_group_children(GroupKind::List, body).unwrap();
    assert_eq!(inner_kids.len(), 1);
    assert_eq!(inner_kids[0].inner_type(), *b"TEXT");
    assert!(!inner_kids[0].is_prop());
}
