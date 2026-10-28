// Query AST produced by the parser and consumed by the compiler.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LogicOp {
    And,
    Or,
}

#[derive(Debug, Clone)]
pub enum Cond {
    Leaf {
        col: String,             // column (may contain ->> / -> json path)
        op: String,              // eq, gt, in, fts, cs, ...
        val: String,             // raw value text
        not: bool,
    },
    Group {
        op: LogicOp,
        not: bool,
        items: Vec<Cond>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Nulls {
    First,
    Last,
}

#[derive(Debug, Clone)]
pub struct OrderItem {
    pub col: String, // may contain json path
    pub dir: Option<Dir>,
    pub nulls: Option<Nulls>,
}

// A single item inside `select=`
#[derive(Debug, Clone)]
pub struct Sel {
    pub name: String,            // column, agg name, or relation for embeds
    pub alias: Option<String>,
    pub cast: Option<String>,    // ::text etc.
    pub json: Option<(String, bool)>, // (json key, is_text_arrow ->> vs ->)
    pub agg: Option<Agg>,        // count/sum/avg/min/max
    pub embed: Option<Box<EmbedNode>>,
}

#[derive(Debug, Clone)]
pub struct Agg {
    pub func: String,           // count|sum|avg|min|max
    pub arg: Option<String>,    // None for count()
}

#[derive(Debug, Clone)]
pub struct EmbedNode {
    pub rel: String,
    pub hint: Option<String>,
    pub inner: bool,
    pub select: Vec<Sel>,
    pub filters: Vec<Cond>,
    pub order: Vec<OrderItem>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub select: Vec<Sel>,        // empty -> all columns
    pub conds: Vec<Cond>,
    pub order: Vec<OrderItem>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub range: Option<crate::headers::RangeHdr>,
}
