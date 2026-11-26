//! Syntax tree for the supported KQL subset.

use wartlog_model::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub table: Table,
    pub ops: Vec<Op>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    Logs,
    Spans,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Where(Expr),
    /// (output name, expression)
    Project(Vec<(String, Expr)>),
    ProjectAway(Vec<String>),
    Extend(Vec<(String, Expr)>),
    Take(u64),
    Sort(Vec<SortKey>),
    Top(u64, SortKey),
    Summarize { aggs: Vec<(String, Agg)>, by: Vec<(String, Expr)> },
    Distinct(Vec<(String, Expr)>),
    Count,
    /// `search "term"`: has-match across body, attributes and resource
    Search(String),
}

impl Op {
    /// Operators that work on one record at a time (allowed in live tail).
    pub fn is_streaming(&self) -> bool {
        matches!(self, Op::Where(_) | Op::Project(_) | Op::ProjectAway(_) | Op::Extend(_) | Op::Search(_))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SortKey {
    pub expr: Expr,
    pub desc: bool,
    pub nulls_first: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Agg {
    pub func: AggFunc,
    pub arg: Option<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    CountIf,
    DCount,
    Min,
    Max,
    Avg,
    Sum,
}

impl AggFunc {
    pub fn name(self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::CountIf => "countif",
            AggFunc::DCount => "dcount",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
            AggFunc::Avg => "avg",
            AggFunc::Sum => "sum",
        }
    }
}

/// Built-in columns. Anything else is a user column (from extend/project/summarize) or, failing
/// that, a record attribute of that name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Column {
    Timestamp,
    ObservedTimestamp,
    SeverityNumber,
    SeverityText,
    Level,
    Body,
    Template,
    Source,
    SourceKind,
    Service,
    Unit,
    Namespace,
    Pod,
    Container,
    TraceId,
    SpanId,
    Id,
    // spans
    Name,
    ParentSpanId,
    EndTimestamp,
    Duration,
    Kind,
    Status,
    StatusMessage,
    /// `Attributes["k"]`
    Attr(String),
    /// `Resource["k"]`
    Resource(String),
    /// whole maps as dynamic values
    Attributes,
    ResourceMap,
    /// a name that is not built in
    Named(String),
}

impl Column {
    pub fn from_name(n: &str) -> Column {
        match n {
            "Timestamp" => Column::Timestamp,
            "ObservedTimestamp" => Column::ObservedTimestamp,
            "SeverityNumber" => Column::SeverityNumber,
            "SeverityText" => Column::SeverityText,
            "Level" => Column::Level,
            "Body" => Column::Body,
            "Template" => Column::Template,
            "Source" => Column::Source,
            "SourceKind" => Column::SourceKind,
            "Service" => Column::Service,
            "Unit" => Column::Unit,
            "Namespace" => Column::Namespace,
            "Pod" => Column::Pod,
            "Container" => Column::Container,
            "TraceId" => Column::TraceId,
            "SpanId" => Column::SpanId,
            "Id" => Column::Id,
            "Name" => Column::Name,
            "ParentSpanId" => Column::ParentSpanId,
            "EndTimestamp" => Column::EndTimestamp,
            "Duration" => Column::Duration,
            "Kind" => Column::Kind,
            "Status" => Column::Status,
            "StatusMessage" => Column::StatusMessage,
            "Attributes" => Column::Attributes,
            "Resource" => Column::ResourceMap,
            other => Column::Named(other.to_string()),
        }
    }

    /// Default output name.
    pub fn name(&self) -> String {
        match self {
            Column::Attr(k) | Column::Resource(k) | Column::Named(k) => k.replace(['.', '-', ' '], "_"),
            Column::ResourceMap => "Resource".into(),
            other => format!("{other:?}"),
        }
    }

    /// True when the value depends only on the record's source descriptor.
    pub fn is_source_level(&self) -> bool {
        matches!(self, Column::Source | Column::SourceKind | Column::Service | Column::Unit | Column::Namespace | Column::Container | Column::Resource(_) | Column::ResourceMap)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    And,
    Or,
    Eq,
    Ne,
    EqCi,
    NeCi,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrOp {
    Has,
    Contains,
    StartsWith,
    EndsWith,
    HasPrefix,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Lit(Value),
    Col(Column),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    /// string operator: (op, case_sensitive, negated)
    Str { op: StrOp, cs: bool, neg: bool, lhs: Box<Expr>, rhs: Box<Expr> },
    In { lhs: Box<Expr>, list: Vec<Expr>, neg: bool, ci: bool },
    Between { lhs: Box<Expr>, lo: Box<Expr>, hi: Box<Expr>, neg: bool },
    Regex { lhs: Box<Expr>, re: Re },
    Call(String, Vec<Expr>),
}

/// A compiled regex, compared by pattern.
#[derive(Clone, Debug)]
pub struct Re(pub std::sync::Arc<regex::Regex>);

impl PartialEq for Re {
    fn eq(&self, o: &Self) -> bool {
        self.0.as_str() == o.0.as_str()
    }
}

impl Expr {
    /// Visits every column reference.
    pub fn columns(&self, f: &mut dyn FnMut(&Column)) {
        match self {
            Expr::Lit(_) => {}
            Expr::Col(c) => f(c),
            Expr::Bin(_, a, b) => {
                a.columns(f);
                b.columns(f);
            }
            Expr::Not(a) | Expr::Neg(a) | Expr::Regex { lhs: a, .. } => a.columns(f),
            Expr::Str { lhs, rhs, .. } => {
                lhs.columns(f);
                rhs.columns(f);
            }
            Expr::In { lhs, list, .. } => {
                lhs.columns(f);
                for e in list {
                    e.columns(f);
                }
            }
            Expr::Between { lhs, lo, hi, .. } => {
                lhs.columns(f);
                lo.columns(f);
                hi.columns(f);
            }
            Expr::Call(_, args) => {
                for a in args {
                    a.columns(f);
                }
            }
        }
    }

    /// Default output name for an unnamed projection (KQL style).
    pub fn default_name(&self, n: usize) -> String {
        match self {
            Expr::Col(c) => c.name(),
            Expr::Call(f, args) => match args.first() {
                Some(Expr::Col(c)) => format!("{f}_{}", c.name()),
                _ => format!("{f}_"),
            },
            _ => format!("Column{n}"),
        }
    }
}
