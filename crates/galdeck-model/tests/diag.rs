use galdeck_model::LineIndex;

#[test]
fn locates_the_first_byte_of_a_file() {
    let index = LineIndex::new("hello\nworld\n");
    let loc = index.locate(0);
    assert_eq!((loc.line, loc.col, loc.utf16), (1, 1, 0));
}

#[test]
fn locates_across_lines() {
    let text = "alpha\nbeta\ngamma\n";
    let index = LineIndex::new(text);
    let at_beta = text.find("beta").unwrap();
    let loc = index.locate(at_beta);
    assert_eq!((loc.line, loc.col), (2, 1));

    let at_gamma = text.find("gamma").unwrap();
    assert_eq!(index.locate(at_gamma).line, 3);
}

#[test]
fn utf16_offsets_account_for_astral_characters() {
    // The reason this exists: JS textarea offsets count UTF-16 code units, so
    // an emoji is 2 there and 4 bytes here. Getting this wrong puts the
    // cursor in the wrong place in any config with non-ASCII labels.
    let text = "label = \"🎹\"\nkey = 0\n";
    let index = LineIndex::new(text);
    let second_line = text.find("key").unwrap();
    let loc = index.locate(second_line);
    assert_eq!(loc.line, 2);
    assert_eq!(loc.col, 1);
    // Line 1 is: l a b e l space = space " 🎹 " \n
    //            9 chars + 2 units for the emoji + 1 quote + 1 newline = 13
    assert_eq!(loc.utf16, 13, "emoji counts as two UTF-16 units");
}

#[test]
fn a_byte_at_the_very_end_still_resolves() {
    let text = "one\ntwo";
    let index = LineIndex::new(text);
    let loc = index.locate(text.len());
    assert_eq!(loc.line, 2);
    assert_eq!(loc.col, 4);
}

#[test]
fn accented_characters_count_as_one_utf16_unit() {
    let text = "café\nx";
    let index = LineIndex::new(text);
    let loc = index.locate(text.find('x').unwrap());
    assert_eq!(loc.line, 2);
    // c a f é \n  -> 4 chars + newline = 5 UTF-16 units, though é is 2 bytes.
    assert_eq!(loc.utf16, 5);
}
