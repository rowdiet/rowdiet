use super::*;
use std::cmp::Ordering::Less;

fn assert_order(files: &[&str]) {
    for pair in files.windows(2) {
        assert_eq!(compare(pair[0], pair[1]), Less, "{} should sort before {}", pair[0], pair[1]);
    }
}

#[test]
fn numeric_not_lexicographic() {
    assert_order(&["V1__a.sql", "V2__b.sql", "V10__c.sql"]);
}

#[test]
fn sub_versions_sort_between() {
    assert_order(&["V1__a.sql", "V1_2__b.sql", "V2__c.sql"]);
    assert_order(&["V1718984460__a.sql", "V1718984460_1__b.sql", "V1718984461__c.sql"]);
    assert_order(&["V1.1__a.sql", "V1.2__b.sql"]);
}

#[test]
fn unversioned_after_versioned() {
    assert_order(&["V9__a.sql", "R__seed_data.sql", "install.sql"]);
}

#[test]
fn prefix_letters() {
    assert_order(&["U1__undo.sql", "V2__x.sql"]);
    assert_order(&["B1__baseline.sql", "V2__x.sql"]);
}

#[test]
fn plain_numeric_prefix() {
    assert_order(&["20240101_init.sql", "20240201_more.sql"]);
}

#[test]
fn versionless_v_is_unversioned() {
    assert_eq!(sort_key("Vx.sql").0, 1);
    assert_eq!(sort_key("R__x.sql").0, 1);
}
