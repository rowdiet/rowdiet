use super::*;

fn t(key: &str) -> TypeRef {
    TypeRef {
        key: key.into(),
        display: key.into(),
        char_len: None,
        dims: 0,
    }
}

fn tn(key: &str, n: u64) -> TypeRef {
    TypeRef {
        key: key.into(),
        display: format!("{key}({n})"),
        char_len: Some(n),
        dims: 0,
    }
}

fn arr(key: &str, dims: u8) -> TypeRef {
    TypeRef {
        key: key.into(),
        display: format!("{key}[]"),
        char_len: None,
        dims,
    }
}

fn cat() -> Catalog {
    Catalog::new(BTreeMap::new())
}

#[test]
fn verified_builtins() {
    assert_eq!(
        cat().resolve(&t("uuid")).kind,
        ColumnKind::Fixed {
            len: 16,
            align: Align::Char
        }
    );
    assert_eq!(
        cat().resolve(&t("int8")).kind,
        ColumnKind::Fixed {
            len: 8,
            align: Align::Double
        }
    );
    assert_eq!(
        cat().resolve(&t("bool")).kind,
        ColumnKind::Fixed {
            len: 1,
            align: Align::Char
        }
    );
    assert!(cat().resolve(&t("timetz")).kind.irregular());
    assert!(cat().resolve(&t("macaddr")).kind.irregular());
    assert_eq!(
        cat().resolve(&t("numeric")).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false
        }
    );
    assert!(cat().resolve(&t("inet")).known);
    assert!(!cat().resolve(&t("inet")).kind.is_fixed());
}

#[test]
fn char_and_varchar_proven_short() {
    assert_eq!(
        cat().resolve(&tn("varchar", 31)).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: true
        }
    );
    assert_eq!(
        cat().resolve(&tn("varchar", 32)).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false
        }
    );
    assert_eq!(
        cat().resolve(&t("varchar")).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false
        }
    );
    assert_eq!(
        cat().resolve(&tn("bpchar", 1)).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: true
        }
    );
}

#[test]
fn quoted_char_byte_type() {
    assert_eq!(
        cat().resolve(&t("pgchar")).kind,
        ColumnKind::Fixed {
            len: 1,
            align: Align::Char
        }
    );
}

#[test]
fn arrays_follow_element_alignment() {
    assert_eq!(
        cat().resolve(&arr("int8", 1)).kind,
        ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false
        }
    );
    assert_eq!(
        cat().resolve(&arr("text", 1)).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false
        }
    );
    assert_eq!(
        cat().resolve(&arr("float8", 2)).kind,
        ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false
        }
    );
    let unknown_elem = cat().resolve(&arr("nope", 1));
    assert_eq!(
        unknown_elem.kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false
        }
    );
    assert!(!unknown_elem.known);
}

#[test]
fn serial_implies_not_null() {
    let r = cat().resolve(&t("bigserial"));
    assert_eq!(
        r.kind,
        ColumnKind::Fixed {
            len: 8,
            align: Align::Double
        }
    );
    assert!(r.implicit_not_null);
    assert!(cat().resolve(&t("serial")).implicit_not_null);
    assert!(!cat().resolve(&t("int4")).implicit_not_null);
}

#[test]
fn unknown_defaults_flagged() {
    let r = cat().resolve(&t("wat_type"));
    assert_eq!(
        r.kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false
        }
    );
    assert!(!r.known);
}

#[test]
fn assume_specs_parse() {
    assert_eq!(
        parse_assume_spec("vector=varlena:d").unwrap(),
        ("vector".to_string(), AssumedKind::Varlena { align: Align::Double })
    );
    assert_eq!(
        parse_assume_spec("Foo=fixed:16:c").unwrap(),
        (
            "foo".to_string(),
            AssumedKind::Fixed {
                len: 16,
                align: Align::Char
            }
        )
    );
    assert!(parse_assume_spec("nope").is_err());
    assert!(parse_assume_spec("x=fixed:banana:c").is_err());
    assert!(parse_assume_spec("x=varlena:z").is_err());
}

#[test]
fn curated_extension_types_are_verified() {
    for key in ["citext", "hstore", "vector", "halfvec", "sparsevec"] {
        let r = cat().resolve(&t(key));
        assert_eq!(
            r.kind,
            ColumnKind::Varlena {
                align: Align::Int,
                proven_short: false
            },
            "{key}"
        );
        assert!(r.known, "{key}");
    }
}

#[test]
fn assume_map_overrides_builtin() {
    let mut assume = BTreeMap::new();
    assume.insert("citext".to_string(), AssumedKind::Varlena { align: Align::Int });
    assume.insert(
        "vector".to_string(),
        AssumedKind::Fixed {
            len: 16,
            align: Align::Double,
        },
    );
    let c = Catalog::new(assume);
    assert!(c.resolve(&t("citext")).known);
    assert_eq!(
        c.resolve(&t("vector")).kind,
        ColumnKind::Fixed {
            len: 16,
            align: Align::Double
        }
    );
}

#[test]
fn session_types() {
    let mut c = cat();
    c.define_enum("status".into());
    assert_eq!(
        c.resolve(&t("status")).kind,
        ColumnKind::Fixed {
            len: 4,
            align: Align::Int
        }
    );
    assert!(c.resolve(&t("status")).known);
    c.define_composite("pair".into());
    assert_eq!(
        c.resolve(&t("pair")).kind,
        ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false
        }
    );
    c.define_range("bigrange".into(), Some(&t("int8")));
    assert_eq!(
        c.resolve(&t("bigrange")).kind,
        ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false
        }
    );
    c.define_range("textrange".into(), Some(&t("text")));
    assert_eq!(
        c.resolve(&t("textrange")).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false
        }
    );
    c.define_domain("code".into(), &tn("varchar", 20));
    assert_eq!(
        c.resolve(&t("code")).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: true
        }
    );
    c.drop_type("status");
    assert!(!c.resolve(&t("status")).known);
}

#[test]
fn enum_array_is_int_aligned_varlena() {
    let mut c = cat();
    c.define_enum("status".into());
    assert_eq!(
        c.resolve(&arr("status", 1)).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false
        }
    );
    assert!(c.resolve(&arr("status", 1)).known);
}
