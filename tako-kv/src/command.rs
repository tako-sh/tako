//! The supported commands: their arity, where they run, and how their
//! arguments parse. Errors use Redis's wording, since client libraries match
//! on some of them.

use bytes::Bytes;

use crate::resp::Reply;

pub(crate) const WRONGTYPE: &str =
    "WRONGTYPE Operation against a key holding the wrong kind of value";
pub(crate) const NOT_INTEGER: &str = "ERR value is not an integer or out of range";
pub(crate) const NOT_FLOAT: &str = "ERR value is not a valid float";
pub(crate) const SYNTAX: &str = "ERR syntax error";
pub(crate) const OVERFLOW: &str = "ERR increment or decrement would overflow";
pub(crate) const NAN_OR_INFINITY: &str = "ERR increment would produce NaN or Infinity";
pub(crate) const HASH_NOT_INTEGER: &str = "ERR hash value is not an integer";
pub(crate) const HASH_NOT_FLOAT: &str = "ERR hash value is not a float";
pub(crate) const NO_SUCH_KEY: &str = "ERR no such key";
pub(crate) const OOM: &str = "OOM command not allowed when used memory > 'maxmemory'.";

/// Where a command runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// Reads the store.
    Read,
    /// Changes the store; `grows` commands are refused when it is full.
    Write { grows: bool },
    /// Handled by the connection.
    Connection,
}

pub(crate) struct Spec {
    pub name: &'static str,
    /// Exact argument count including the name when positive; the minimum
    /// when negative.
    pub arity: i32,
    pub class: Class,
    /// The ACL category of its kind, for `COMMAND INFO`.
    category: &'static str,
}

const R: Class = Class::Read;
const W: Class = Class::Write { grows: false };
const G: Class = Class::Write { grows: true };
const C: Class = Class::Connection;

const S: &str = "@string";
const K: &str = "@keyspace";
const H: &str = "@hash";
const X: &str = "@connection";
const T: &str = "@transaction";
const A: &str = "@admin";
const D: &str = "@dangerous";
const F: &str = "@fast";

const SPECS: &[Spec] = &[
    // Strings
    spec("get", 2, R, S),
    spec("set", -3, G, S),
    spec("setnx", 3, G, S),
    spec("setex", 4, G, S),
    spec("psetex", 4, G, S),
    spec("getdel", 2, W, S),
    spec("getex", -2, W, S),
    spec("getset", 3, G, S),
    spec("mget", -2, R, S),
    spec("mset", -3, G, S),
    spec("msetnx", -3, G, S),
    spec("append", 3, G, S),
    spec("strlen", 2, R, S),
    spec("incr", 2, G, S),
    spec("decr", 2, G, S),
    spec("incrby", 3, G, S),
    spec("decrby", 3, G, S),
    spec("incrbyfloat", 3, G, S),
    // Keys
    spec("del", -2, W, K),
    spec("unlink", -2, W, K),
    spec("exists", -2, R, K),
    spec("touch", -2, R, K),
    spec("type", 2, R, K),
    spec("expire", -3, W, K),
    spec("pexpire", -3, W, K),
    spec("expireat", -3, W, K),
    spec("pexpireat", -3, W, K),
    spec("ttl", 2, R, K),
    spec("pttl", 2, R, K),
    spec("expiretime", 2, R, K),
    spec("pexpiretime", 2, R, K),
    spec("persist", 2, W, K),
    spec("rename", 3, W, K),
    spec("renamenx", 3, W, K),
    spec("keys", 2, R, K),
    spec("scan", -2, R, K),
    spec("dbsize", 1, R, K),
    spec("flushdb", -1, W, K),
    spec("flushall", -1, W, K),
    // Hashes
    spec("hset", -4, G, H),
    spec("hmset", -4, G, H),
    spec("hsetnx", 4, G, H),
    spec("hget", 3, R, H),
    spec("hmget", -3, R, H),
    spec("hgetall", 2, R, H),
    spec("hkeys", 2, R, H),
    spec("hvals", 2, R, H),
    spec("hlen", 2, R, H),
    spec("hexists", 3, R, H),
    spec("hdel", -3, W, H),
    spec("hincrby", 4, G, H),
    spec("hincrbyfloat", 4, G, H),
    spec("hstrlen", 3, R, H),
    spec("hscan", -3, R, H),
    // Connection and server
    spec("ping", -1, C, X),
    spec("echo", 2, C, X),
    spec("select", 2, C, X),
    spec("quit", -1, C, X),
    spec("reset", 1, C, X),
    spec("auth", -2, C, X),
    spec("hello", -1, C, X),
    spec("client", -2, C, X),
    spec("command", -1, C, X),
    spec("info", -1, C, D),
    spec("config", -2, C, A),
    spec("time", 1, C, F),
    spec("wait", 3, C, X),
    spec("multi", 1, C, T),
    spec("exec", 1, C, T),
    spec("discard", 1, C, T),
    spec("watch", -2, C, T),
    spec("unwatch", 1, C, T),
];

const fn spec(name: &'static str, arity: i32, class: Class, category: &'static str) -> Spec {
    Spec {
        name,
        arity,
        class,
        category,
    }
}

impl Spec {
    /// The first key, the last (negative counts from the end), and the step
    /// between keys, as `COMMAND INFO` reports them; zeros for no keys.
    fn key_positions(&self) -> (i64, i64, i64) {
        match self.name {
            _ if self.class == Class::Connection && self.name != "watch" => (0, 0, 0),
            "keys" | "scan" | "dbsize" | "flushdb" | "flushall" => (0, 0, 0),
            "mget" | "del" | "unlink" | "exists" | "touch" | "watch" => (1, -1, 1),
            "mset" | "msetnx" => (1, -1, 2),
            "rename" | "renamenx" => (1, 2, 1),
            _ => (1, 1, 1),
        }
    }

    /// The command's `COMMAND INFO` entry, shaped like Redis 7's: name,
    /// arity, flags, key positions, ACL categories, then tips, key
    /// specifications and subcommands, which are empty here.
    pub(crate) fn info(&self) -> Reply {
        let (flags, kind): (&[&str], _) = match self.class {
            Class::Read => (&["readonly"], Some("@read")),
            Class::Write { grows: true } => (&["write", "denyoom"], Some("@write")),
            Class::Write { grows: false } => (&["write"], Some("@write")),
            Class::Connection => (&[], None),
        };
        let simple = |text: &str| Reply::Simple(text.to_owned());
        let (first, last, step) = self.key_positions();
        Reply::Array(vec![
            Reply::bulk_str(self.name),
            Reply::Int(i64::from(self.arity)),
            Reply::Set(flags.iter().copied().map(simple).collect()),
            Reply::Int(first),
            Reply::Int(last),
            Reply::Int(step),
            Reply::Set(
                kind.into_iter()
                    .chain([self.category])
                    .map(simple)
                    .collect(),
            ),
            Reply::Set(Vec::new()),
            Reply::Set(Vec::new()),
            Reply::Set(Vec::new()),
        ])
    }
}

/// Every command the server knows, in table order.
pub(crate) fn all() -> &'static [Spec] {
    SPECS
}

/// The command named `name`, in any case.
pub(crate) fn find(name: &[u8]) -> Option<&'static Spec> {
    SPECS
        .iter()
        .find(|spec| spec.name.as_bytes().eq_ignore_ascii_case(name))
}

/// Number of commands the server knows, for `COMMAND COUNT`.
pub(crate) fn count() -> usize {
    SPECS.len()
}

/// Looks up a command and checks its argument count, returning the error
/// reply Redis gives otherwise.
pub(crate) fn lookup(args: &[Bytes]) -> Result<&'static Spec, Reply> {
    let name = &args[0];
    let Some(spec) = find(name) else {
        let mut message = format!(
            "ERR unknown command '{}', with args beginning with: ",
            printable(name, 128)
        );
        for arg in args[1..].iter().take(16) {
            message.push_str(&format!("'{}' ", printable(arg, 128)));
        }
        return Err(Reply::Error(message));
    };
    let n = args.len() as i32;
    if (spec.arity > 0 && n != spec.arity) || (spec.arity < 0 && n < -spec.arity) {
        return Err(wrong_arguments(spec.name));
    }
    Ok(spec)
}

pub(crate) fn wrong_arguments(name: &str) -> Reply {
    Reply::Error(format!(
        "ERR wrong number of arguments for '{name}' command"
    ))
}

fn printable(bytes: &[u8], limit: usize) -> String {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(limit)]);
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// An expiry time, resolved against the clock when the command runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum When {
    /// Milliseconds from now.
    In(i64),
    /// Unix time in milliseconds.
    At(i64),
}

impl When {
    /// The absolute time, or `None` when it does not fit.
    pub(crate) fn resolve(self, now: u64) -> Option<i64> {
        match self {
            Self::In(ms) => (now as i64).checked_add(ms),
            Self::At(ms) => Some(ms),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetCondition {
    Always,
    IfMissing,
    IfExists,
}

/// What a write does to the key's expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ttl {
    /// Removes it.
    Clear,
    /// Keeps it.
    Keep,
    Set(When),
}

/// `EXPIRE` options. They combine as in Redis: `XX` with `GT` or `LT`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExpireIf {
    /// Only without an expiry.
    pub none: bool,
    /// Only with one.
    pub some: bool,
    /// Only later than the current one; no expiry counts as infinite.
    pub later: bool,
    /// Only sooner than the current one.
    pub sooner: bool,
}

impl ExpireIf {
    /// Whether to replace `current` (`None` for no expiry) with `when`.
    pub(crate) fn allows(self, current: Option<i64>, when: i64) -> bool {
        !((self.none && current.is_some())
            || (self.some && current.is_none())
            || (self.later && current.is_none_or(|current| when <= current))
            || (self.sooner && current.is_some_and(|current| when >= current)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TtlQuery {
    Seconds,
    Millis,
    /// `EXPIRETIME`
    UnixSeconds,
    /// `PEXPIRETIME`
    UnixMillis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyType {
    String,
    Hash,
}

impl KeyType {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Hash => "hash",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Cmd {
    Get(Bytes),
    Set {
        key: Bytes,
        value: Bytes,
        condition: SetCondition,
        get: bool,
        ttl: Ttl,
        /// The command name, for error messages.
        name: &'static str,
    },
    /// `SETNX`, which replies with an integer.
    SetNx {
        key: Bytes,
        value: Bytes,
    },
    GetDel(Bytes),
    GetEx {
        key: Bytes,
        ttl: Ttl,
    },
    MGet(Vec<Bytes>),
    MSet {
        pairs: Vec<(Bytes, Bytes)>,
        only_new: bool,
    },
    Append {
        key: Bytes,
        value: Bytes,
    },
    Strlen(Bytes),
    IncrBy {
        key: Bytes,
        by: i64,
    },
    IncrByFloat {
        key: Bytes,
        by: f64,
    },
    Del(Vec<Bytes>),
    Exists(Vec<Bytes>),
    Type(Bytes),
    Expire {
        key: Bytes,
        when: When,
        condition: ExpireIf,
        name: &'static str,
    },
    Ttl {
        key: Bytes,
        query: TtlQuery,
    },
    Persist(Bytes),
    Rename {
        from: Bytes,
        to: Bytes,
        only_new: bool,
    },
    Keys(Bytes),
    Scan {
        cursor: u64,
        pattern: Option<Bytes>,
        count: usize,
        kind: Option<Option<KeyType>>,
    },
    DbSize,
    /// `FLUSHDB`/`FLUSHALL`; `ASYNC` frees the old data in the background.
    Flush {
        lazy: bool,
    },
    HSet {
        key: Bytes,
        pairs: Vec<(Bytes, Bytes)>,
        /// `HMSET` replies `OK`.
        ok: bool,
    },
    HSetNx {
        key: Bytes,
        field: Bytes,
        value: Bytes,
    },
    HGet {
        key: Bytes,
        field: Bytes,
    },
    HMGet {
        key: Bytes,
        fields: Vec<Bytes>,
    },
    HGetAll(Bytes),
    HKeys(Bytes),
    HVals(Bytes),
    HLen(Bytes),
    HExists {
        key: Bytes,
        field: Bytes,
    },
    HDel {
        key: Bytes,
        fields: Vec<Bytes>,
    },
    HIncrBy {
        key: Bytes,
        field: Bytes,
        by: i64,
    },
    HIncrByFloat {
        key: Bytes,
        field: Bytes,
        by: f64,
    },
    HStrlen {
        key: Bytes,
        field: Bytes,
    },
    HScan {
        key: Bytes,
        cursor: u64,
        pattern: Option<Bytes>,
        count: usize,
        values: bool,
    },
}

impl Cmd {
    /// The largest reply the command can have, at most `max_reply`, so a
    /// transaction can reserve room for its replies before it runs.
    pub(crate) fn reply_bound(&self, limits: Limits, max_reply: usize) -> usize {
        /// Enough for any reply but a stored value: numbers, `OK`, errors.
        const SMALL: usize = 256;
        let value = limits.max_value_bytes + 16;
        match self {
            Self::Get(_)
            | Self::GetDel(_)
            | Self::GetEx { .. }
            | Self::HGet { .. }
            | Self::Set { get: true, .. } => value,
            Self::MGet(keys) => 16 + keys.len().saturating_mul(value),
            Self::HMGet { fields, .. } => 16 + fields.len().saturating_mul(value),
            Self::HGetAll(_)
            | Self::HKeys(_)
            | Self::HVals(_)
            | Self::Keys(_)
            | Self::Scan { .. }
            | Self::HScan { .. } => max_reply,
            _ => SMALL,
        }
        .min(max_reply)
    }

    pub(crate) fn writes(&self) -> bool {
        !matches!(
            self,
            Self::Get(_)
                | Self::MGet(_)
                | Self::Strlen(_)
                | Self::Exists(_)
                | Self::Type(_)
                | Self::Ttl { .. }
                | Self::Keys(_)
                | Self::Scan { .. }
                | Self::DbSize
                | Self::HGet { .. }
                | Self::HMGet { .. }
                | Self::HGetAll(_)
                | Self::HKeys(_)
                | Self::HVals(_)
                | Self::HLen(_)
                | Self::HExists { .. }
                | Self::HStrlen { .. }
                | Self::HScan { .. }
        )
    }

    /// Keys and fields the command names, for size limits.
    fn names(&self) -> Vec<&Bytes> {
        match self {
            Self::Get(k)
            | Self::GetDel(k)
            | Self::Strlen(k)
            | Self::Type(k)
            | Self::Persist(k)
            | Self::HGetAll(k)
            | Self::HKeys(k)
            | Self::HVals(k)
            | Self::HLen(k)
            | Self::GetEx { key: k, .. }
            | Self::Set { key: k, .. }
            | Self::SetNx { key: k, .. }
            | Self::Append { key: k, .. }
            | Self::IncrBy { key: k, .. }
            | Self::IncrByFloat { key: k, .. }
            | Self::Expire { key: k, .. }
            | Self::Ttl { key: k, .. }
            | Self::HScan { key: k, .. } => vec![k],
            Self::MGet(keys) | Self::Del(keys) | Self::Exists(keys) => keys.iter().collect(),
            Self::MSet { pairs, .. } => pairs.iter().map(|(k, _)| k).collect(),
            Self::Rename { from, to, .. } => vec![from, to],
            Self::HSet { key, pairs, .. } => std::iter::once(key)
                .chain(pairs.iter().map(|(f, _)| f))
                .collect(),
            Self::HSetNx { key, field, .. }
            | Self::HGet { key, field }
            | Self::HExists { key, field }
            | Self::HIncrBy { key, field, .. }
            | Self::HIncrByFloat { key, field, .. }
            | Self::HStrlen { key, field } => vec![key, field],
            Self::HMGet { key, fields } | Self::HDel { key, fields } => {
                std::iter::once(key).chain(fields.iter()).collect()
            }
            Self::Keys(_) | Self::Scan { .. } | Self::DbSize | Self::Flush { .. } => Vec::new(),
        }
    }

    /// Values the command stores, for size limits.
    fn values(&self) -> Vec<&Bytes> {
        match self {
            Self::Set { value, .. }
            | Self::SetNx { value, .. }
            | Self::Append { value, .. }
            | Self::HSetNx { value, .. } => vec![value],
            Self::MSet { pairs, .. } | Self::HSet { pairs, .. } => {
                pairs.iter().map(|(_, v)| v).collect()
            }
            _ => Vec::new(),
        }
    }
}

/// Limits a command's keys, fields and values must fit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub max_key_bytes: usize,
    pub max_value_bytes: usize,
}

/// Parses a store command's arguments; `args[0]` is its name.
pub(crate) fn parse(spec: &Spec, args: &[Bytes], limits: Limits) -> Result<Cmd, Reply> {
    let cmd = parse_args(spec.name, args)?;
    if cmd.names().iter().any(|n| n.len() > limits.max_key_bytes) {
        return Err(Reply::Error(format!(
            "ERR keys and hash fields are limited to {} bytes",
            limits.max_key_bytes
        )));
    }
    if cmd
        .values()
        .iter()
        .any(|v| v.len() > limits.max_value_bytes)
    {
        return Err(Reply::Error(format!(
            "ERR values are limited to {} bytes",
            limits.max_value_bytes
        )));
    }
    Ok(cmd)
}

fn err(message: &str) -> Reply {
    Reply::Error(message.to_owned())
}

/// Parses an integer the way Redis does: no sign but `-`, no spaces, no
/// leading zeros.
pub(crate) fn int(bytes: &[u8]) -> Option<i64> {
    let digits = bytes.strip_prefix(b"-").unwrap_or(bytes);
    if digits.is_empty()
        || bytes.len() > 20
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits[0] == b'0' && (digits.len() > 1 || bytes.len() > 1))
    {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// Parses a float: no surrounding spaces, and never NaN.
pub(crate) fn float(bytes: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.is_empty() || text.trim() != text {
        return None;
    }
    let value: f64 = text.parse().ok()?;
    (!value.is_nan()).then_some(value)
}

fn int_arg(bytes: &[u8]) -> Result<i64, Reply> {
    int(bytes).ok_or_else(|| err(NOT_INTEGER))
}

fn float_arg(bytes: &[u8]) -> Result<f64, Reply> {
    float(bytes).ok_or_else(|| err(NOT_FLOAT))
}

fn invalid_expire(name: &str) -> Reply {
    Reply::Error(format!("ERR invalid expire time in '{name}' command"))
}

/// An expiry option's argument: positive, and in milliseconds.
fn expire_arg(value: &[u8], seconds: bool, name: &str) -> Result<i64, Reply> {
    let value = int_arg(value)?;
    if value <= 0 {
        return Err(invalid_expire(name));
    }
    if seconds {
        value.checked_mul(1000).ok_or_else(|| invalid_expire(name))
    } else {
        Ok(value)
    }
}

fn is(arg: &[u8], option: &str) -> bool {
    arg.eq_ignore_ascii_case(option.as_bytes())
}

fn pairs(args: &[Bytes], name: &str) -> Result<Vec<(Bytes, Bytes)>, Reply> {
    if args.is_empty() || !args.len().is_multiple_of(2) {
        return Err(wrong_arguments(name));
    }
    Ok(args
        .chunks(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect())
}

fn parse_args(name: &'static str, args: &[Bytes]) -> Result<Cmd, Reply> {
    let a = args;
    Ok(match name {
        "get" => Cmd::Get(a[1].clone()),
        "set" => parse_set(a)?,
        "setnx" => Cmd::SetNx {
            key: a[1].clone(),
            value: a[2].clone(),
        },
        "setex" | "psetex" => Cmd::Set {
            key: a[1].clone(),
            value: a[3].clone(),
            condition: SetCondition::Always,
            get: false,
            ttl: Ttl::Set(When::In(expire_arg(&a[2], name == "setex", name)?)),
            name,
        },
        "getset" => Cmd::Set {
            key: a[1].clone(),
            value: a[2].clone(),
            condition: SetCondition::Always,
            get: true,
            ttl: Ttl::Clear,
            name,
        },
        "getdel" => Cmd::GetDel(a[1].clone()),
        "getex" => {
            let mut ttl = Ttl::Keep;
            let mut i = 2;
            while i < a.len() {
                let option = &a[i];
                if ttl != Ttl::Keep {
                    return Err(err(SYNTAX));
                }
                if is(option, "persist") {
                    ttl = Ttl::Clear;
                    i += 1;
                    continue;
                }
                let value = a.get(i + 1).ok_or_else(|| err(SYNTAX))?;
                ttl = Ttl::Set(if is(option, "ex") {
                    When::In(expire_arg(value, true, name)?)
                } else if is(option, "px") {
                    When::In(expire_arg(value, false, name)?)
                } else if is(option, "exat") {
                    When::At(expire_arg(value, true, name)?)
                } else if is(option, "pxat") {
                    When::At(expire_arg(value, false, name)?)
                } else {
                    return Err(err(SYNTAX));
                });
                i += 2;
            }
            Cmd::GetEx {
                key: a[1].clone(),
                ttl,
            }
        }
        "mget" => Cmd::MGet(a[1..].to_vec()),
        "mset" | "msetnx" => Cmd::MSet {
            pairs: pairs(&a[1..], name)?,
            only_new: name == "msetnx",
        },
        "append" => Cmd::Append {
            key: a[1].clone(),
            value: a[2].clone(),
        },
        "strlen" => Cmd::Strlen(a[1].clone()),
        "incr" => Cmd::IncrBy {
            key: a[1].clone(),
            by: 1,
        },
        "decr" => Cmd::IncrBy {
            key: a[1].clone(),
            by: -1,
        },
        "incrby" => Cmd::IncrBy {
            key: a[1].clone(),
            by: int_arg(&a[2])?,
        },
        "decrby" => Cmd::IncrBy {
            key: a[1].clone(),
            by: int_arg(&a[2])?
                .checked_neg()
                .ok_or_else(|| err("ERR decrement would overflow"))?,
        },
        "incrbyfloat" => Cmd::IncrByFloat {
            key: a[1].clone(),
            by: float_arg(&a[2])?,
        },
        "del" | "unlink" => Cmd::Del(a[1..].to_vec()),
        "exists" | "touch" => Cmd::Exists(a[1..].to_vec()),
        "type" => Cmd::Type(a[1].clone()),
        "expire" | "pexpire" | "expireat" | "pexpireat" => parse_expire(name, a)?,
        "ttl" | "pttl" | "expiretime" | "pexpiretime" => Cmd::Ttl {
            key: a[1].clone(),
            query: match name {
                "ttl" => TtlQuery::Seconds,
                "pttl" => TtlQuery::Millis,
                "expiretime" => TtlQuery::UnixSeconds,
                _ => TtlQuery::UnixMillis,
            },
        },
        "persist" => Cmd::Persist(a[1].clone()),
        "rename" | "renamenx" => Cmd::Rename {
            from: a[1].clone(),
            to: a[2].clone(),
            only_new: name == "renamenx",
        },
        "keys" => Cmd::Keys(a[1].clone()),
        "scan" => parse_scan(a)?,
        "dbsize" => Cmd::DbSize,
        "flushdb" | "flushall" => {
            if a.len() > 2 || a.get(1).is_some_and(|m| !is(m, "sync") && !is(m, "async")) {
                return Err(err(SYNTAX));
            }
            Cmd::Flush {
                lazy: a.get(1).is_some_and(|m| is(m, "async")),
            }
        }
        "hset" | "hmset" => Cmd::HSet {
            key: a[1].clone(),
            pairs: pairs(&a[2..], name)?,
            ok: name == "hmset",
        },
        "hsetnx" => Cmd::HSetNx {
            key: a[1].clone(),
            field: a[2].clone(),
            value: a[3].clone(),
        },
        "hget" => Cmd::HGet {
            key: a[1].clone(),
            field: a[2].clone(),
        },
        "hmget" => Cmd::HMGet {
            key: a[1].clone(),
            fields: a[2..].to_vec(),
        },
        "hgetall" => Cmd::HGetAll(a[1].clone()),
        "hkeys" => Cmd::HKeys(a[1].clone()),
        "hvals" => Cmd::HVals(a[1].clone()),
        "hlen" => Cmd::HLen(a[1].clone()),
        "hexists" => Cmd::HExists {
            key: a[1].clone(),
            field: a[2].clone(),
        },
        "hdel" => Cmd::HDel {
            key: a[1].clone(),
            fields: a[2..].to_vec(),
        },
        "hincrby" => Cmd::HIncrBy {
            key: a[1].clone(),
            field: a[2].clone(),
            by: int_arg(&a[3])?,
        },
        "hincrbyfloat" => Cmd::HIncrByFloat {
            key: a[1].clone(),
            field: a[2].clone(),
            by: float_arg(&a[3])?,
        },
        "hstrlen" => Cmd::HStrlen {
            key: a[1].clone(),
            field: a[2].clone(),
        },
        "hscan" => {
            let cursor = cursor_arg(&a[2])?;
            let mut pattern = None;
            let mut count = 10;
            let mut values = true;
            let mut i = 3;
            while i < a.len() {
                if is(&a[i], "match") && i + 1 < a.len() {
                    pattern = Some(a[i + 1].clone());
                    i += 2;
                } else if is(&a[i], "count") && i + 1 < a.len() {
                    count = count_arg(&a[i + 1])?;
                    i += 2;
                } else if is(&a[i], "novalues") {
                    values = false;
                    i += 1;
                } else {
                    return Err(err(SYNTAX));
                }
            }
            Cmd::HScan {
                key: a[1].clone(),
                cursor,
                pattern,
                count,
                values,
            }
        }
        _ => unreachable!("store command {name} has no parser"),
    })
}

/// Options may repeat, as in Redis, but not conflict: `NX` with `XX`, or
/// `KEEPTTL` and the expiry options with each other.
fn parse_set(a: &[Bytes]) -> Result<Cmd, Reply> {
    let name = "set";
    let mut condition = SetCondition::Always;
    let mut get = false;
    let mut ttl = Ttl::Clear;
    let mut i = 3;
    while i < a.len() {
        let option = &a[i];
        let expiry = ["ex", "px", "exat", "pxat"]
            .into_iter()
            .find(|o| is(option, o));
        if is(option, "nx") && condition != SetCondition::IfExists {
            condition = SetCondition::IfMissing;
        } else if is(option, "xx") && condition != SetCondition::IfMissing {
            condition = SetCondition::IfExists;
        } else if is(option, "get") {
            get = true;
        } else if is(option, "keepttl") && matches!(ttl, Ttl::Clear | Ttl::Keep) {
            ttl = Ttl::Keep;
        } else if let Some(expiry) = expiry
            && let Some(value) = a.get(i + 1)
            && match ttl {
                Ttl::Clear => true,
                Ttl::Keep => false,
                // The same option may repeat; the last value wins.
                Ttl::Set(When::In(_)) => a[..i]
                    .iter()
                    .rev()
                    .find(|o| ["ex", "px"].iter().any(|e| is(o, e)))
                    .is_some_and(|o| is(o, expiry)),
                Ttl::Set(When::At(_)) => a[..i]
                    .iter()
                    .rev()
                    .find(|o| ["exat", "pxat"].iter().any(|e| is(o, e)))
                    .is_some_and(|o| is(o, expiry)),
            }
        {
            let seconds = expiry == "ex" || expiry == "exat";
            let ms = expire_arg(value, seconds, name)?;
            ttl = Ttl::Set(if expiry == "ex" || expiry == "px" {
                When::In(ms)
            } else {
                When::At(ms)
            });
            i += 1;
        } else {
            return Err(err(SYNTAX));
        }
        i += 1;
    }
    Ok(Cmd::Set {
        key: a[1].clone(),
        value: a[2].clone(),
        condition,
        get,
        ttl,
        name,
    })
}

fn parse_expire(name: &'static str, a: &[Bytes]) -> Result<Cmd, Reply> {
    let amount = int_arg(&a[2])?;
    let seconds = name == "expire" || name == "expireat";
    let ms = if seconds {
        amount
            .checked_mul(1000)
            .ok_or_else(|| invalid_expire(name))?
    } else {
        amount
    };
    let when = if name.ends_with("at") {
        When::At(ms)
    } else {
        When::In(ms)
    };
    let mut condition = ExpireIf::default();
    for option in &a[3..] {
        if is(option, "nx") {
            condition.none = true;
        } else if is(option, "xx") {
            condition.some = true;
        } else if is(option, "gt") {
            condition.later = true;
        } else if is(option, "lt") {
            condition.sooner = true;
        } else {
            return Err(Reply::Error(format!(
                "ERR Unsupported option {}",
                String::from_utf8_lossy(option)
            )));
        }
    }
    if condition.none && (condition.some || condition.later || condition.sooner) {
        return Err(err(
            "ERR NX and XX, GT or LT options at the same time are not compatible",
        ));
    }
    if condition.later && condition.sooner {
        return Err(err(
            "ERR GT and LT options at the same time are not compatible",
        ));
    }
    Ok(Cmd::Expire {
        key: a[1].clone(),
        when,
        condition,
        name,
    })
}

fn cursor_arg(arg: &[u8]) -> Result<u64, Reply> {
    std::str::from_utf8(arg)
        .ok()
        .filter(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|c| c.parse::<u64>().ok())
        .ok_or_else(|| err("ERR invalid cursor"))
}

/// A `COUNT` hint, capped so one page cannot take long.
fn count_arg(arg: &[u8]) -> Result<usize, Reply> {
    let n = int_arg(arg)?;
    if n < 1 {
        return Err(err(SYNTAX));
    }
    Ok(n.min(100_000) as usize)
}

fn parse_scan(a: &[Bytes]) -> Result<Cmd, Reply> {
    let cursor = cursor_arg(&a[1])?;
    let mut pattern = None;
    let mut count = 10;
    let mut kind = None;
    let mut i = 2;
    while i < a.len() {
        let value = a.get(i + 1).ok_or_else(|| err(SYNTAX))?;
        if is(&a[i], "match") {
            pattern = Some(value.clone());
        } else if is(&a[i], "count") {
            count = count_arg(value)?;
        } else if is(&a[i], "type") {
            kind = Some(if is(value, "string") {
                Some(KeyType::String)
            } else if is(value, "hash") {
                Some(KeyType::Hash)
            } else {
                // Other types exist in Redis, but no key here has them.
                None
            });
        } else {
            return Err(err(SYNTAX));
        }
        i += 2;
    }
    Ok(Cmd::Scan {
        cursor,
        pattern,
        count,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<Bytes> {
        text.split(' ')
            .map(|a| Bytes::copy_from_slice(a.as_bytes()))
            .collect()
    }

    fn try_parse(text: &str) -> Result<Cmd, Reply> {
        let args = args(text);
        let spec = lookup(&args)?;
        parse(
            spec,
            &args,
            Limits {
                max_key_bytes: 16,
                max_value_bytes: 32,
            },
        )
    }

    fn error(text: &str) -> String {
        match try_parse(text) {
            Err(Reply::Error(message)) => message,
            other => panic!("{text}: {other:?}"),
        }
    }

    #[test]
    fn integers_parse_like_redis() {
        assert_eq!(int(b"0"), Some(0));
        assert_eq!(int(b"-12"), Some(-12));
        assert_eq!(int(b"9223372036854775807"), Some(i64::MAX));
        for bad in [
            "",
            "-",
            "+1",
            " 1",
            "01",
            "-0",
            "1.0",
            "9223372036854775808",
        ] {
            assert_eq!(int(bad.as_bytes()), None, "{bad}");
        }
        assert_eq!(float(b"1e3"), Some(1000.0));
        assert_eq!(float(b"nan"), None);
        assert_eq!(float(b" 1"), None);
    }

    #[test]
    fn reports_redis_errors() {
        assert_eq!(
            error("foo a b"),
            "ERR unknown command 'foo', with args beginning with: 'a' 'b' "
        );
        assert_eq!(
            error("get a b"),
            "ERR wrong number of arguments for 'get' command"
        );
        assert_eq!(error("set k v NX XX"), SYNTAX);
        assert_eq!(error("set k v EX 10 PX 10"), SYNTAX);
        assert_eq!(error("set k v KEEPTTL EX 1"), SYNTAX);
        assert_eq!(
            error("set k v EX 0"),
            "ERR invalid expire time in 'set' command"
        );
        assert_eq!(
            error("set k v EX 9223372036854775807"),
            "ERR invalid expire time in 'set' command"
        );
        assert_eq!(error("setex k abc v"), NOT_INTEGER);
        assert_eq!(
            error("psetex k -5 v"),
            "ERR invalid expire time in 'psetex' command"
        );
        assert_eq!(error("getex k PERSIST EX 1"), SYNTAX);
        assert_eq!(
            error("getex k PX 0"),
            "ERR invalid expire time in 'getex' command"
        );
        assert_eq!(
            error("expire k 10 NX XX"),
            "ERR NX and XX, GT or LT options at the same time are not compatible"
        );
        assert_eq!(
            error("expire k 10 GT LT"),
            "ERR GT and LT options at the same time are not compatible"
        );
        assert_eq!(
            error("expire k 9223372036854775807"),
            "ERR invalid expire time in 'expire' command"
        );
        assert_eq!(
            error("mset a 1 b"),
            "ERR wrong number of arguments for 'mset' command"
        );
        assert_eq!(
            error("hset h f v g"),
            "ERR wrong number of arguments for 'hset' command"
        );
        assert_eq!(error("scan abc"), "ERR invalid cursor");
        assert_eq!(error("scan 0 COUNT 0"), SYNTAX);
        assert_eq!(error("incrbyfloat f abc"), NOT_FLOAT);
        assert_eq!(
            error("get aaaaaaaaaaaaaaaaa"),
            "ERR keys and hash fields are limited to 16 bytes"
        );
        assert_eq!(
            error("set k aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "ERR values are limited to 32 bytes"
        );
    }

    #[test]
    fn parses_set_options() {
        assert_eq!(
            try_parse("SET k v nx get px 1500").unwrap(),
            Cmd::Set {
                key: "k".into(),
                value: "v".into(),
                condition: SetCondition::IfMissing,
                get: true,
                ttl: Ttl::Set(When::In(1500)),
                name: "set",
            }
        );
        assert_eq!(
            try_parse("SET k v EXAT 10").unwrap(),
            Cmd::Set {
                key: "k".into(),
                value: "v".into(),
                condition: SetCondition::Always,
                get: false,
                ttl: Ttl::Set(When::At(10_000)),
                name: "set",
            }
        );
        assert!(matches!(
            try_parse("hscan h 7 count 3 novalues").unwrap(),
            Cmd::HScan {
                cursor: 7,
                count: 3,
                values: false,
                ..
            }
        ));
        assert_eq!(
            try_parse("hscan h x").unwrap_err(),
            err("ERR invalid cursor")
        );
        assert_eq!(try_parse("hscan h 0 count 0").unwrap_err(), err(SYNTAX));
        assert!(matches!(
            try_parse("scan 5 match a* count 3 type hash").unwrap(),
            Cmd::Scan {
                cursor: 5,
                count: 3,
                kind: Some(Some(KeyType::Hash)),
                ..
            }
        ));
    }
}
