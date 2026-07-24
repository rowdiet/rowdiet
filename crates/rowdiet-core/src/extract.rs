//! The only module that touches the sqlparser AST: one statement's text → dialect-neutral
//! [`DdlOp`]s. Parse failures are returned as errors for the caller to surface loudly.

use crate::catalog::TypeRef;
use sqlparser::ast as sq;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawName {
    pub display: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawColumn {
    pub display: String,
    pub key: String,
    pub type_ref: TypeRef,
    pub not_null: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DdlOp {
    CreateTable {
        name: RawName,
        columns: Vec<RawColumn>,
        pk_columns: Vec<String>,
        if_not_exists: bool,
        is_ctas: bool,
        /// LIKE / INHERITS / typed table — declared columns are not the whole story.
        incomplete_columns: bool,
        /// CREATE TEMPORARY TABLE — session-lived, excluded from analysis.
        temporary: bool,
        /// `PARTITION OF parent`: the child's physical layout is the parent's, verbatim.
        partition_of: Option<RawName>,
    },
    AddColumn {
        table: RawName,
        column: RawColumn,
        if_not_exists: bool,
    },
    DropColumns {
        table: RawName,
        columns: Vec<String>,
        if_exists: bool,
    },
    RenameColumn {
        table: RawName,
        old: String,
        new: String,
    },
    RenameTable {
        table: RawName,
        new: RawName,
    },
    SetColumnType {
        table: RawName,
        column: String,
        type_ref: TypeRef,
    },
    SetNotNull {
        table: RawName,
        column: String,
        value: bool,
    },
    DropTables {
        names: Vec<RawName>,
        if_exists: bool,
    },
    CreateEnum {
        name: RawName,
    },
    CreateComposite {
        name: RawName,
    },
    CreateRange {
        name: RawName,
        subtype: Option<TypeRef>,
    },
    CreateBase {
        name: RawName,
    },
    CreateDomain {
        name: RawName,
        base: TypeRef,
    },
    DropTypes {
        names: Vec<RawName>,
    },
    RenameType {
        name: RawName,
        new: RawName,
    },
    Irrelevant,
}

pub fn extract(text: &str) -> Result<Vec<DdlOp>, String> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, text).map_err(|e| e.to_string())?;
    Ok(statements.into_iter().flat_map(map_statement).collect())
}

fn map_statement(stmt: sq::Statement) -> Vec<DdlOp> {
    match stmt {
        sq::Statement::CreateTable(ct) => vec![map_create_table(ct)],
        sq::Statement::AlterTable(at) => map_alter_table(at),
        sq::Statement::CreateType { name, representation } => vec![map_create_type(&name, representation)],
        sq::Statement::AlterType(at) => match at.operation {
            sq::AlterTypeOperation::Rename(rename) => vec![DdlOp::RenameType {
                name: raw_name(&at.name),
                new: RawName {
                    display: rename.new_name.value.clone(),
                    key: ident_key(&rename.new_name),
                },
            }],
            _ => vec![DdlOp::Irrelevant],
        },
        sq::Statement::CreateDomain(cd) => {
            vec![DdlOp::CreateDomain {
                name: raw_name(&cd.name),
                base: map_type(&cd.data_type),
            }]
        }
        sq::Statement::Drop {
            object_type,
            names,
            if_exists,
            ..
        } => match object_type {
            sq::ObjectType::Table => {
                vec![DdlOp::DropTables {
                    names: names.iter().map(raw_name).collect(),
                    if_exists,
                }]
            }
            sq::ObjectType::Type => vec![DdlOp::DropTypes {
                names: names.iter().map(raw_name).collect(),
            }],
            _ => vec![DdlOp::Irrelevant],
        },
        _ => vec![DdlOp::Irrelevant],
    }
}

fn map_create_table(ct: sq::CreateTable) -> DdlOp {
    let is_ctas = ct.query.is_some() && ct.columns.is_empty();
    let incomplete_columns = ct.like.is_some() || ct.inherits.is_some();
    let partition_of = ct.partition_of.as_ref().map(raw_name);
    let pk_columns = ct.constraints.iter().flat_map(pk_constraint_columns).collect();
    let columns = ct.columns.iter().map(map_column).collect();
    DdlOp::CreateTable {
        name: raw_name(&ct.name),
        columns,
        pk_columns,
        if_not_exists: ct.if_not_exists,
        is_ctas,
        incomplete_columns,
        temporary: ct.temporary,
        partition_of,
    }
}

fn pk_constraint_columns(constraint: &sq::TableConstraint) -> Vec<String> {
    match constraint {
        sq::TableConstraint::PrimaryKey(pk) => pk.columns.iter().filter_map(index_column_key).collect(),
        _ => Vec::new(),
    }
}

fn index_column_key(ic: &sq::IndexColumn) -> Option<String> {
    match &ic.column.expr {
        sq::Expr::Identifier(id) => Some(ident_key(id)),
        sq::Expr::CompoundIdentifier(ids) => ids.last().map(ident_key),
        _ => None,
    }
}

fn map_column(cd: &sq::ColumnDef) -> RawColumn {
    let not_null = cd.options.iter().any(|od| match &od.option {
        sq::ColumnOption::NotNull => true,
        sq::ColumnOption::PrimaryKey(_) => true,
        // Identity columns (no generation expression) are implicitly NOT NULL.
        sq::ColumnOption::Generated {
            generation_expr: None, ..
        } => true,
        _ => false,
    });
    RawColumn {
        display: cd.name.value.clone(),
        key: ident_key(&cd.name),
        type_ref: map_type(&cd.data_type),
        not_null,
    }
}

fn map_alter_table(at: sq::AlterTable) -> Vec<DdlOp> {
    let table = raw_name(&at.name);
    at.operations
        .into_iter()
        .flat_map(|op| map_alter_op(&table, op))
        .collect()
}

fn map_alter_op(table: &RawName, op: sq::AlterTableOperation) -> Vec<DdlOp> {
    match op {
        sq::AlterTableOperation::AddColumn {
            if_not_exists,
            column_def,
            ..
        } => {
            vec![DdlOp::AddColumn {
                table: table.clone(),
                column: map_column(&column_def),
                if_not_exists,
            }]
        }
        sq::AlterTableOperation::DropColumn {
            column_names,
            if_exists,
            ..
        } => {
            let columns = column_names.iter().map(ident_key).collect();
            vec![DdlOp::DropColumns {
                table: table.clone(),
                columns,
                if_exists,
            }]
        }
        sq::AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => {
            vec![DdlOp::RenameColumn {
                table: table.clone(),
                old: ident_key(&old_column_name),
                new: ident_key(&new_column_name),
            }]
        }
        sq::AlterTableOperation::RenameTable { table_name } => {
            let new = match table_name {
                sq::RenameTableNameKind::As(n) => raw_name(&n),
                sq::RenameTableNameKind::To(n) => raw_name(&n),
            };
            vec![DdlOp::RenameTable {
                table: table.clone(),
                new,
            }]
        }
        sq::AlterTableOperation::AlterColumn { column_name, op } => {
            map_alter_column(table, ident_key(&column_name), op)
        }
        // ADD PRIMARY KEY forces NOT NULL on its columns.
        sq::AlterTableOperation::AddConstraint { constraint, .. } => pk_constraint_columns(&constraint)
            .into_iter()
            .map(|column| DdlOp::SetNotNull {
                table: table.clone(),
                column,
                value: true,
            })
            .collect(),
        _ => vec![DdlOp::Irrelevant],
    }
}

fn map_alter_column(table: &RawName, column: String, op: sq::AlterColumnOperation) -> Vec<DdlOp> {
    match op {
        sq::AlterColumnOperation::SetNotNull => {
            vec![DdlOp::SetNotNull {
                table: table.clone(),
                column,
                value: true,
            }]
        }
        sq::AlterColumnOperation::DropNotNull => {
            vec![DdlOp::SetNotNull {
                table: table.clone(),
                column,
                value: false,
            }]
        }
        sq::AlterColumnOperation::SetDataType { data_type, .. } => {
            vec![DdlOp::SetColumnType {
                table: table.clone(),
                column,
                type_ref: map_type(&data_type),
            }]
        }
        sq::AlterColumnOperation::SetDefault { .. }
        | sq::AlterColumnOperation::DropDefault
        | sq::AlterColumnOperation::AddGenerated { .. } => vec![DdlOp::Irrelevant],
    }
}

fn map_create_type(name: &sq::ObjectName, representation: Option<sq::UserDefinedTypeRepresentation>) -> DdlOp {
    let name = raw_name(name);
    match representation {
        Some(sq::UserDefinedTypeRepresentation::Enum { .. }) => DdlOp::CreateEnum { name },
        Some(sq::UserDefinedTypeRepresentation::Composite { .. }) => DdlOp::CreateComposite { name },
        Some(sq::UserDefinedTypeRepresentation::Range { options }) => {
            let subtype = options.iter().find_map(|o| match o {
                sq::UserDefinedTypeRangeOption::Subtype(dt) => Some(map_type(dt)),
                _ => None,
            });
            DdlOp::CreateRange { name, subtype }
        }
        Some(sq::UserDefinedTypeRepresentation::SqlDefinition { .. }) | None => DdlOp::CreateBase { name },
    }
}

/// Postgres folds unquoted identifiers to lowercase; quoted ones are verbatim.
fn ident_key(id: &sq::Ident) -> String {
    match id.quote_style {
        None => id.value.to_lowercase(),
        Some(_) => id.value.clone(),
    }
}

/// Tables and types are keyed by their last name component (search_path resolution is out of
/// scope); the full original spelling is kept for display.
fn raw_name(name: &sq::ObjectName) -> RawName {
    let key = name
        .0
        .last()
        .and_then(|part| part.as_ident())
        .map(ident_key)
        .unwrap_or_else(|| name.to_string().to_lowercase());
    RawName {
        display: name.to_string(),
        key,
    }
}

fn map_type(dt: &sq::DataType) -> TypeRef {
    let display = dt.to_string();
    if let sq::DataType::Array(elem) = dt {
        let inner = match elem {
            sq::ArrayElemTypeDef::SquareBracket(inner, _) => Some(inner),
            sq::ArrayElemTypeDef::AngleBracket(inner) => Some(inner),
            sq::ArrayElemTypeDef::Parenthesis(inner) => Some(inner),
            sq::ArrayElemTypeDef::None => None,
        };
        return match inner {
            Some(inner) => {
                let base = map_type(inner);
                TypeRef {
                    key: base.key,
                    display,
                    char_len: None,
                    dims: base.dims.saturating_add(1),
                }
            }
            None => TypeRef {
                key: "array".into(),
                display,
                char_len: None,
                dims: 1,
            },
        };
    }
    let (key, char_len) = scalar_key(dt);
    TypeRef {
        key,
        display,
        char_len,
        dims: 0,
    }
}

fn scalar_key(dt: &sq::DataType) -> (String, Option<u64>) {
    use sq::DataType::*;
    let key = |s: &str| (s.to_string(), None);
    match dt {
        Boolean | Bool => key("bool"),
        SmallInt(_) | Int2(_) => key("int2"),
        Int(_) | Integer(_) | Int4(_) => key("int4"),
        // int8 means eight BYTES in Postgres (bigint), not eight bits.
        BigInt(_) | Int8(_) => key("int8"),
        Real | Float4 => key("float4"),
        DoublePrecision | Float8 => key("float8"),
        Float(info) => match info {
            sq::ExactNumberInfo::Precision(p) if *p <= 24 => key("float4"),
            _ => key("float8"),
        },
        Numeric(_) | Decimal(_) | Dec(_) => key("numeric"),
        // Bare `char` is char(1) in Postgres; bare varchar stays unlimited.
        Char(l) | Character(l) => ("bpchar".to_string(), char_len(l).or(Some(1))),
        Varchar(l) | CharacterVarying(l) | CharVarying(l) => ("varchar".to_string(), char_len(l)),
        Text => key("text"),
        Uuid => key("uuid"),
        Date => key("date"),
        Time(_, tz) => match tz {
            sq::TimezoneInfo::WithTimeZone | sq::TimezoneInfo::Tz => key("timetz"),
            _ => key("time"),
        },
        Timestamp(_, tz) => match tz {
            sq::TimezoneInfo::WithTimeZone | sq::TimezoneInfo::Tz => key("timestamptz"),
            _ => key("timestamp"),
        },
        Interval { .. } => key("interval"),
        JSON => key("json"),
        JSONB => key("jsonb"),
        Bytea => key("bytea"),
        Bit(_) => key("bit"),
        BitVarying(_) | VarBit(_) => key("varbit"),
        TsVector => key("tsvector"),
        TsQuery => key("tsquery"),
        Regclass => key("regclass"),
        GeometricType(g) => key(geometric_key(g)),
        Custom(name, _) => (custom_key(name), None),
        other => (other.to_string().to_lowercase(), None),
    }
}

fn char_len(l: &Option<sq::CharacterLength>) -> Option<u64> {
    match l {
        Some(sq::CharacterLength::IntegerLength { length, .. }) => Some(*length),
        Some(sq::CharacterLength::Max) => None,
        None => None,
    }
}

fn geometric_key(g: &sq::GeometricTypeKind) -> &'static str {
    match g {
        sq::GeometricTypeKind::Point => "point",
        sq::GeometricTypeKind::Line => "line",
        sq::GeometricTypeKind::LineSegment => "lseg",
        sq::GeometricTypeKind::GeometricBox => "box",
        sq::GeometricTypeKind::GeometricPath => "path",
        sq::GeometricTypeKind::Polygon => "polygon",
        sq::GeometricTypeKind::Circle => "circle",
    }
}

fn custom_key(name: &sq::ObjectName) -> String {
    match name.0.last().and_then(|part| part.as_ident()) {
        // The quoted one-byte `"char"` type is distinct from char(1)/bpchar.
        Some(id) if id.quote_style == Some('"') && id.value == "char" => "pgchar".to_string(),
        Some(id) => ident_key(id),
        None => name.to_string().to_lowercase(),
    }
}

/// sqlparser 0.62 cannot parse `CREATE UNLOGGED TABLE`; stripping the keyword is layout-neutral.
pub fn preprocess(text: &str) -> String {
    let words = word_spans(text, 3);
    let is = |r: &Range<usize>, w: &str| text[r.clone()].eq_ignore_ascii_case(w);
    if words.len() == 3 && is(&words[0], "create") && is(&words[1], "unlogged") && is(&words[2], "table") {
        let mut stripped = String::with_capacity(text.len());
        stripped.push_str(&text[..words[1].start]);
        stripped.push_str(&text[words[2].start..]);
        return stripped;
    }
    text.to_string()
}

fn word_spans(text: &str, n: usize) -> Vec<Range<usize>> {
    let b = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < b.len() && spans.len() < n {
        if b[i].is_ascii_whitespace() {
            i += 1;
        } else if b[i] == b'_' || b[i].is_ascii_alphanumeric() {
            let start = i;
            while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            spans.push(start..i);
        } else {
            break;
        }
    }
    spans
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sniff {
    CreateTable(String),
    AlterTable(String),
}

/// Best-effort identification of the table an *unparseable* statement targets, so the skip note
/// can say whose analysis is incomplete.
pub fn sniff(text: &str) -> Option<Sniff> {
    let tokens = sniff_tokens(text, 8);
    let mut it = tokens.iter();
    let (first, _) = it.next()?;
    if first.eq_ignore_ascii_case("alter") {
        let (kw, _) = it.next()?;
        if !kw.eq_ignore_ascii_case("table") {
            return None;
        }
        let mut tok = it.next()?;
        if tok.0.eq_ignore_ascii_case("only") {
            tok = it.next()?;
        }
        if tok.0.eq_ignore_ascii_case("if") {
            it.next()?;
            tok = it.next()?;
        }
        if !tok.1 && tok.0.ends_with('.') {
            tok = it.next()?;
        }
        return Some(Sniff::AlterTable(sniff_key(tok)));
    }
    if first.eq_ignore_ascii_case("create") {
        let mut tok = it.next()?;
        while ["global", "local", "temporary", "temp", "unlogged"]
            .iter()
            .any(|w| tok.0.eq_ignore_ascii_case(w))
        {
            tok = it.next()?;
        }
        if !tok.0.eq_ignore_ascii_case("table") {
            return None;
        }
        let mut name = it.next()?;
        if name.0.eq_ignore_ascii_case("if") {
            it.next()?;
            it.next()?;
            name = it.next()?;
        }
        if !name.1 && name.0.ends_with('.') {
            // `schema."Quoted Name"` tokenizes as an unquoted `schema.` followed by the quoted
            // identifier; the quoted token is the table name.
            name = it.next()?;
        }
        return Some(Sniff::CreateTable(sniff_key(name)));
    }
    None
}

fn sniff_key((token, quoted): &(String, bool)) -> String {
    if *quoted {
        token.clone()
    } else {
        token.rsplit('.').next().unwrap_or(token).to_lowercase()
    }
}

fn sniff_tokens(text: &str, max: usize) -> Vec<(String, bool)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() && out.len() < max {
        let c = b[i];
        if c == b'"' {
            let start = i + 1;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            out.push((text[start..i.min(text.len())].to_string(), true));
            i += 1;
        } else if c == b'_' || c == b'$' || c == b'.' || c.is_ascii_alphanumeric() {
            let start = i;
            while i < b.len() && (b[i] == b'_' || b[i] == b'$' || b[i] == b'.' || b[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            out.push((text[start..i].to_string(), false));
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests;
