// Regenerate deliberately with: INSTA_UPDATE=always cargo test -p vici --test editor_cases

use vici::fixtures::{parse_cases, run_case};

#[test]
fn editor_cases() {
    let mut snapshot = String::new();
    for case in parse_cases(include_str!("fixtures/editor.vici")) {
        let block =
            run_case(&case).unwrap_or_else(|error| panic!("{}: invalid keys: {error}", case.name));
        snapshot.push_str(&block);
    }
    insta::assert_snapshot!(snapshot);
}
